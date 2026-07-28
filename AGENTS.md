# Agentyk — coding-agent guidance

Agentyk is a standalone Rust library for composing agents from values and
running them (turn loop, event log, capabilities, MCP, multi-provider
drivers). It reuses the domain language of
[`everruns`](https://github.com/everruns/everruns) but is built from scratch;
the long-term plan is to rebuild everruns-core/runtime on top of it. Read
[`knowledge/plan.md`](knowledge/plan.md) before changing architecture.

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
  replay. Lean by construction: **no tokio, no HTTP, no process spawning**.
  Anything a third party implements or serializes lives here. Physical files
  and public modules use the same domain names.
- `crates/agentyk-engine` — the one canonical step engine: `Agent`, `Session`,
  middleware/policy orchestration, prepared model/tool-batch operations, and
  the in-process host. Durable execution hosts this engine; it never copies
  the turn loop.
- `crates/agentyk` — the facade and bundled modules: `JsonlEventLog`, drivers
  (feature `http`), MCP (feature `mcp`), filesystem (feature `fs`). MCP and
  filesystem are first-class library capabilities, not integration crates.
  Re-exports core and engine explicitly; applications depend only on
  `agentyk`. `python3 scripts/check_reexports.py` enforces the facade surface.

- `examples/<name>` — runnable applications (`publish = false`). Workspace
  members so they build and test in CI, but never a dependency of the crates.
  Free to take deps the library would not (a TUI toolkit, an argument parser).

Drivers and capabilities stay as feature-gated modules in `agentyk`.
Extracting another crate requires a concrete dependency, ownership, or release
need. Do not add tokio/reqwest/process deps to core or bundled integrations to
engine.

## Design rules (enforced in review)

- Every public item is documented — `missing_docs` is `deny`, so this is
  enforced by the compiler, not by review. Say what an item is *for*; a doc
  that restates the name is worse than none.
- Values first: no API that requires creating-then-referencing an entity by
  id. Ids are outputs, never inputs.
- The event log is the persistence seam; replay must suffice to resume a session.
- Traits stay host-neutral: nothing in core presumes a database, server,
  or tenant.
- Heavy integrations go behind features (`http`, `mcp`) or arrive as capabilities.
- Keep the everruns vocabulary (see the mapping table in `knowledge/plan.md`).

## Knowledge and docs

- `knowledge/` is the repository’s persistent memory and durable design intent —
  the **why** and **what**, not exhaustive **how** (link to source for exact
  fields/enums/API shapes). It is an [Open Knowledge Format v0.2](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md)
  bundle: one concept per markdown file with a `type` in its frontmatter,
  listed in [`knowledge/index.md`](knowledge/index.md). Read the relevant
  knowledge before changing behavior in that area; integrate new decisions and
  update stale knowledge as part of the same change.
- Public, task-oriented documentation for users lives in the top-level
  `README.md`. Do not put internal design intent, roadmaps, or gap analyses
  there — those are specifications expressed as knowledge concepts.
- New or changed technical diagrams follow [`knowledge/diagrams.md`](knowledge/diagrams.md):
  co-located Mermaid source plus hand-authored SVG, embedded via the SVG, and
  rasterized for visual review before shipping.
- Follow [`knowledge/maintenance.md`](knowledge/maintenance.md) for the
  definition of done and knowledge-maintenance rules. Validate the bundle after
  editing it with the canonical linter:
  `okf-lint lint knowledge && okf-lint fmt --check knowledge`. Install it with
  `cargo install okf-lint --version 0.1.1 --locked`.
- `.claude/skills/` holds workflows requestable by name: `/ship` lands a change
  (bar in [`knowledge/shipping.md`](knowledge/shipping.md), workflow in the skill).

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

## Evals

[`evals/`](evals/README.md) holds the model-driven studies — what the library
does when a real model drives it (tool use, tool choice, approval-gate
compliance, tokens, prompt-cache reuse, latency). They are
[Mira](https://github.com/everruns/mira) studies, live outside the Cargo
workspace, and depend on the crates by path. The offline eval runs on the
scripted `SimDriver` with no key and is part of CI; the paid presets are run by
hand. Bar and rationale: [`knowledge/evals.md`](knowledge/evals.md).

```sh
cd evals/agentyk_basic && cargo test        # the offline eval
cd evals/agentyk_basic && mira run --preset smoke   # needs a provider key
```

## Git and commits

- Conventional Commits: `type(scope): description`.
- Types: `feat`, `fix`, `docs`, `refactor`, `test`, `chore`.
- Never add Claude/session/AI attribution links in commits, PRs, docs, or code comments.
- Stage files explicitly by name. Avoid broad `git add .` / `git add -A`.
- Commit attribution must be a real human user. If git identity is missing or
  agent-like, set it from `GIT_USER_NAME` / `GIT_USER_EMAIL`; if those are
  absent, stop and ask before committing.
