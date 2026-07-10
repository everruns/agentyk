# Agentyk — coding-agent guidance

Agentyk is a standalone Rust library for composing agents from values and
running them (turn loop, event log, capabilities, MCP, multi-provider
drivers). It reuses the domain language of
[`everruns`](https://github.com/everruns/everruns) but is built from scratch;
the long-term plan is to rebuild everruns-core/runtime on top of it. Read
[`docs/plan.md`](docs/plan.md) before changing architecture.

## Workflow

- Telegraph. Drop filler. Keep updates short and factual.
- Fix the root cause. If unsure, read more code; if still stuck, ask with short options.
- Keep changes small, PR-sized, testable, and runnable offline.
- For bug fixes, write or update a failing test before the fix when practical.
- Important decisions belong as concise comments near the relevant code.
- No backward compatibility required — agentyk is pre-0.1 on crates.io.

## Design rules (enforced in review)

- Values first: no API that requires creating-then-referencing an entity by
  id. Ids are outputs, never inputs.
- The event log is the persistence seam; replay must suffice to resume a session.
- Traits stay host-neutral: nothing in this crate presumes a database, server,
  or tenant.
- Heavy integrations go behind features (`http`, `mcp`) or arrive as capabilities.
- Keep the everruns vocabulary (see the mapping table in `docs/plan.md`).

## Local dev and tests

Everything runs offline — tests use the scripted `SimDriver` and a canned
stdio MCP server; no API keys needed.

```sh
cargo test --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
cargo run --example hello
```

## Git and commits

- Conventional Commits: `type(scope): description`.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
- Never add Claude/session/AI attribution links in commits, PRs, docs, or code comments.
- Stage files explicitly by name. Avoid broad `git add .` / `git add -A`.
- Commit attribution must be a real human user. If git identity is missing or
  agent-like, set it from `GIT_USER_NAME` / `GIT_USER_EMAIL`; if those are
  absent, stop and ask before committing.
