
## Important Rule

### KISS and First Principles

Follow the KISS principle and reason from first principles during development. Start by identifying the real problem, required behavior, and smallest useful change before adding code. Do not pile on features, configuration switches, abstractions, dependencies, or compatibility layers unless they directly solve the current problem and have clear evidence of need.

Prefer the simplest implementation that is correct, maintainable, and consistent with the existing codebase. If a broader design seems attractive, reduce it to the essential behavior needed now and leave optional expansion for a later, explicit requirement.

### No Unnecessary Helpers

Prioritize inline implementation over abstraction. Avoid over-engineering and do not create helper functions unless absolutely necessary.

1. **Inline-First Rule**: If a logic block can be implemented directly within the main function without breaking overall readability, **do not** extract it into a new helper function.
2. **Strict Justification for Helpers**: You may only create a separate helper function if it meets at least one of these criteria:
   - **High Reuse**: The exact same logic is repeated across **3 or more** different locations.
   - **Extreme Complexity**: Inlining the logic makes the main function too long (e.g., >50 lines) or severely derails the main execution flow.
3. **No Fragmentation**: Do not split continuous linear logic (e.g., a single API call, simple form validation, or one-time data formatting) into tiny functions just for the sake of "clean code."
4. **Keep Context Compact**: Handle edge cases, error catching, and logging directly inside the main function block instead of offloading them.
5. **Refactoring Constraint**: When modifying existing code, do not alter the current function structure or extract code into new helpers unless the existing code already violates the complexity or reuse rules above.


## Required reading

Start with the human docs to understand what Nota is and how it works:

- `README.md` / `README.zh.md` — what Nota does, quick start, OneBot setup
- `CONTRIBUTING.md` — the system reference: architecture, HTTP API,
  directory layout, hard rules & pitfalls

Then read `.agent/README.md` before making changes.

**One fact lives in exactly one doc — reference, don't restate.** Each
behavioral fact has a single home inside `.agent`; `.agent/README.md` is
the index. `CONTRIBUTING.md` is the overview and hard-rule index and never
restates a `.agent` fact. When a behavior changes, edit only the owning
`.agent` file (plus code docstrings in the changed code); if you catch
yourself editing several docs for one fact, remove the copies and keep the
pointer.

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
  `CONTRIBUTING.md` and the READMEs are human-facing: they must never
  reference agent-facing docs (`AGENTS.md`, `.agent/*`). `AGENTS.md` may
  point at `.agent` or `.agent/README.md` — never a specific topic file or
  topic name.
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
loop. Crate table and terminology (session vs conversation):
`CONTRIBUTING.md` → Architecture overview / Terminology.

## General lessons

- When the user says "add", don't replace existing types — keep both.
- Don't auto-add defaults (persona, …) unless explicitly requested; don't
  hardcode names that belong in filesystem config.
- Don't over-engineer minimal asks.
- Idiomatic English in logs and user-facing strings.
