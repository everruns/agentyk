# Agentyk — coding-agent guidance

Agentyk is a standalone Rust library for composing agents from values and
running them (turn loop, event log, capabilities, MCP, multi-provider
drivers). It reuses the domain language of
[`everruns`](https://github.com/everruns/everruns) but is built from scratch;
the long-term plan is to rebuild everruns-core/runtime on top of it. Read
[`specs/plan.md`](specs/plan.md) before changing architecture.

## Workflow

- Telegraph. Drop filler. Keep updates short and factual.
- Fix the root cause. If unsure, read more code; if still stuck, ask with short options.
- Keep changes small, PR-sized, testable, and runnable offline.
- For bug fixes, write or update a failing test before the fix when practical.
- Important decisions belong as concise comments near the relevant code.
- No backward compatibility required — agentyk is pre-0.1 on crates.io.

## Packaging

Cargo workspace, lockstep versions:

- `crates/agentyk-core` — the contract: traits, event protocol, turn machine,
  atoms, replay. Lean by construction: **no tokio, no HTTP, no process
  spawning**. Anything a third party implements or serializes lives here.
  Files are grouped into `protocol/`, `agent/` and `runtime/`, but every
  module is re-exported at the crate root — directories organize the source,
  they are never public paths.
- `crates/agentyk` — the framework: builders, `InProcessExecutor`,
  `JsonlEventLog`, bundled drivers (feature `http`), MCP (feature `mcp`).
  Re-exports all of core **explicitly, not by glob**; applications depend only
  on `agentyk`. When you add a public item to core, add it to that list —
  `python3 scripts/check_reexports.py` (and CI) fails otherwise.

- `examples/<name>` — runnable applications (`publish = false`). Workspace
  members so they build and test in CI, but never a dependency of the crates.
  Free to take deps the library would not (a TUI toolkit, an argument parser).

New drivers/capabilities start as feature-gated modules in `agentyk` and
graduate to `agentyk-<name>` satellite crates (depending only on core) when
they grow a heavy dependency. Do not add tokio/reqwest/process deps to core.

## Design rules (enforced in review)

- Values first: no API that requires creating-then-referencing an entity by
  id. Ids are outputs, never inputs.
- The event log is the persistence seam; replay must suffice to resume a session.
- Traits stay host-neutral: nothing in core presumes a database, server,
  or tenant.
- Heavy integrations go behind features (`http`, `mcp`) or arrive as capabilities.
- Keep the everruns vocabulary (see the mapping table in `specs/plan.md`).

## Specs and docs

- `specs/` is durable design intent for maintainers — the **why** and **what**,
  not exhaustive **how** (link to source for exact fields/enums/API shapes). It
  is an [Open Knowledge Format](https://okf.md) v0.1 bundle: one concept per
  markdown file with a `type` in its frontmatter, listed in
  [`specs/index.md`](specs/index.md). Read the relevant spec before changing
  behavior in that area; add or update one when intent changes.
- Public, task-oriented documentation for users lives in the top-level
  `README.md`. Do not put internal design intent, roadmaps, or gap analyses
  there — those are specs.
- Validate the bundle with yolop's zero-dependency checker when editing specs:
  `python3 <yolop>/src/bundled/system-skills/okf/scripts/validate_okf.py specs --strict`.
- `.claude/skills/` holds workflows requestable by name: `/ship` lands a change
  (bar in [`specs/shipping.md`](specs/shipping.md), workflow in the skill).

## Local dev and tests

Everything runs offline — tests use the scripted `SimDriver` and a canned
stdio MCP server; no API keys needed.

```sh
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo fmt --all --check
cargo run -p agentyk --example hello
python3 scripts/check_reexports.py
```

## Git and commits

- Conventional Commits: `type(scope): description`.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
- Never add Claude/session/AI attribution links in commits, PRs, docs, or code comments.
- Stage files explicitly by name. Avoid broad `git add .` / `git add -A`.
- Commit attribution must be a real human user. If git identity is missing or
  agent-like, set it from `GIT_USER_NAME` / `GIT_USER_EMAIL`; if those are
  absent, stop and ask before committing.
