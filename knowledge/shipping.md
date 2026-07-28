---
type: Process
title: Shipping process
description: Defines the evidence and quality bar required before a repository change ships.
tags: [shipping, review, testing, ci]
---

# Shipping process

## Status

Implemented.

## Abstract

Shipping means completing the requested change, gathering proof that matches the
risk it carries, opening a mergeable PR, and merging only when the PR and main
are fully green.

Adopted from [yolop](https://github.com/everruns/yolop)'s shipping process and
reduced to agentyk's shape. Two differences drive the reduction. agentyk is a
**library**, so the risk surface is its contract — core purity, feature gates,
protocol compatibility — not a host's disk and shell. And everything here runs
**offline** on the scripted `SimDriver` and a canned stdio MCP server, so a test
is always possible and "it needs a key" is never a reason to skip one.

The agent workflow lives in [`.claude/skills/ship/SKILL.md`](../.claude/skills/ship/SKILL.md),
which is invocable as `/ship`.

## Design goals

1. Reach the requested goal, not perform rituals around it.
2. Match validation depth to the risk actually changed.
3. Keep affected artifacts in sync (`README.md`, `AGENTS.md`, `knowledge/`, `CHANGELOG.md`).
4. Merge only from a safe branch state with fully green PR and main CI.

## Ownership boundary

- This spec owns the intent, the constraints, and the bar a shipped change clears.
- The skill owns the workflow: order of operations, heuristics, and commands.

## Required outcomes

Every shipped change MUST satisfy all of these.

1. **Safe branch and healthy main.** Never ship from `main`. The working tree is
   clean before the final push, the branch is up to date with `origin/main`, and
   the latest CI run for `main` is green before merge. A pending or red main
   build blocks the merge even when the PR itself is green.
2. **Goal achieved with reproducible evidence.** The requested behavior is
   implemented, and the proof is the smallest thing that would convince a
   skeptical reviewer. A fix reproduces the reported failure before the change
   and proves the corrected behavior after it. Preserve that before/after proof
   in an automated test plus the most useful review artifact: CLI/API output,
   logs, screenshots, or a VHS recording for terminal or timing-sensitive
   behavior. State why an artifact is not useful when the test is the complete
   reproduction.
3. **Behavior extensively covered at the changed seam.** Every behavior change
   gets automated tests that drive the real entry point — a turn through the
   executor, a fold of the event stream, a driver's wire mapping — not a
   constructor or adjacent code that still compiles. Cover the meaningful
   success, failure, boundary, regression, and integration paths exposed by the
   change; do not chase a numeric coverage target or duplicate equivalent
   assertions. Tests stay offline; the `SimDriver` and canned MCP server make
   that always possible. Docs- or config-only changes are exempt with stated
   justification.
4. **Impact and public API reviewed.** Record who and what the change affects,
   compatibility and migration consequences, alternatives considered when the
   shape is not obvious, and the result of the public-API review below. A
   breaking change may be correct before 0.1, but it must be deliberate,
   documented, and reflected in the changelog.
5. **Security and performance reviewed.** Perform the self-review below and
   record findings, mitigations, measurements when warranted, or a specific
   reason each dimension is not applicable.
6. **Contract reviewed.** The library-risk review below is performed and its
   result recorded in the PR body.
7. **Public docs and durable specs agree.** Update rustdoc for every new or
   changed public API, including a runnable example when usage is not obvious;
   `README.md` for user-facing tasks; an example, demo, or recording when it
   materially helps a user evaluate the behavior; `AGENTS.md` for agent
   guidance; `knowledge/` plus [`index.md`](index.md) when intent changes; and
   `CHANGELOG.md` under `[Unreleased]`. Explicitly record which were updated or
   why each was not needed. No code-duplicating prose.
8. **Smoke test the affected surface.** Beyond the automated test, run the flow:
   `cargo run -p agentyk --example hello` for framework changes, a live-provider
   run for anything touching the `http` drivers, the example binary itself for
   changes under `examples/`. Docs- or config-only changes may state why not.
9. **Follow-ups surfaced.** Deferred work, partial fixes, declined suggestions,
   and known drift go under **Follow-ups** in the PR body, one line of rationale
   each — or "No follow-ups."
10. **Only fully green merges.** The PR uses the template; every required check
    has completed successfully with none pending, skipped unexpectedly, or red;
    and every review comment is answered inline on its own thread and resolved
    — including nits, low-confidence suggestions, and bot comments, and
    including when the resolution is a pure code change. Squash-only, after a
    final clean comment sweep.

## Impact and public API review

Mandatory before merge. Describe affected callers, crates, features, persisted
data, and operational paths. Then inspect every public item added, changed, or
removed:

- Prefer the smallest API that expresses the behavior by value and fits the
  existing domain vocabulary.
- Treat public traits, enum variants, generic bounds, feature availability,
  defaults, error behavior, and serialization shapes as contract changes, not
  implementation details.
- Identify source, behavior, and protocol compatibility separately. If a break
  is deliberate, explain why it is better than an additive design and give
  migration guidance in rustdoc, the README, or the changelog as appropriate.
- Check facade re-exports and run `python3 scripts/check_reexports.py` whenever
  the public surface moves.

Docs-, comment-, or test-only changes may record "No public-API impact" with a
one-line justification.

## Security and performance self-review

Review the dimensions the diff can affect; do not turn "self-review" into an
unexplained checkbox.

Security review includes untrusted model, tool, MCP, HTTP, filesystem, and
serialized input; credential or sensitive-data exposure; path, command, and
network boundaries; dependency and supply-chain risk; denial of service and
resource limits; and any `unsafe` code. Add a regression test for a corrected or
newly defended boundary.

Performance review includes hot-path allocations and cloning, blocking work,
concurrency and backpressure, network or process round trips, event-log growth
and replay, and unbounded collections or payloads. Benchmark or otherwise
measure when the diff creates a plausible material regression; record the
baseline, result, and workload. Do not claim an improvement without a
measurement.

Record each review in the PR body even when the conclusion is "No
security-relevant changes" or "No performance-relevant changes," followed by
the reason.

## Contract review

Mandatory for every change touching code, manifests, or CI. These categories
are the architectural promises the crates make; they complement rather than
replace the security and performance review. For each one the change touches,
look for the failure in the diff:

- **Core purity.** `agentyk-core` gains no tokio, HTTP, or process dependency;
  verify with `cargo tree -p agentyk-core --edges normal`.
- **Feature gates.** Heavy integrations stay behind `http` / `mcp` / `fs`, and
  the lean build still compiles: `cargo clippy -p agentyk --no-default-features
  --lib --tests`.
- **Protocol compatibility.** New event, message, and tool-definition fields are
  additive and serde-optional, so existing JSONL logs still deserialize and
  replay still reconstructs a session's history.
- **Host neutrality.** No trait in core starts presuming a database, a server,
  or a tenant.
- **Secrets.** `ModelSpec` carries an API key by value and serializes it when
  set, so it must not reach an event, a log line, or a serialized message.
- **Dependency risk.** A new crate needs a one-line justification. Core,
  engine, and facade move in lockstep — see [`release.md`](release.md).

Docs-only, comment-only, or test-only changes may record "No contract-relevant
changes" with a one-line justification.

## Constraints

- Shipping is outcome-oriented, not a linear checklist to walk in order.
- Validation starts with the smallest high-signal proof and deepens only where
  risk demands it.
- The contract review is not waived by a change looking low-risk.
- Public API, security, and performance reviews are not waived; an inapplicable
  result needs a concrete reason.
- Auto-merge is not used: reviewer bots can post after the last push or after CI
  turns green. Give them a couple of minutes before merging.
- A blocker the agent cannot resolve safely alone — a merge conflict it cannot
  judge, missing credentials, ambiguous intent, a CI failure it cannot reproduce
  — stops shipping and gets reported rather than guessed at.

## Validation depth

Start from the four checks in [`AGENTS.md`](../AGENTS.md), then add what the
changed surface demands. Two CI steps are wider than the local defaults and are
worth running before pushing, because both fail the build:

- `cargo clippy -p agentyk --no-default-features --lib --tests -- -D warnings`
- `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features`

Manifest or packaging changes also run `scripts/verify_crates_publish.py` per
[`release.md`](release.md). Coverage must prove the change across its meaningful
positive, negative, boundary, regression, and integration paths.

## Merge discipline

- Conventional Commits PR titles, under 70 characters.
- Squash and merge only.
- GitHub Actions is the source of truth. Never merge while the PR or latest main
  CI is red, pending, cancelled, or unexpectedly skipped.
- After merging, watch main's CI for the merge commit; a failure there is a
  shipping regression, to be fixed or reverted promptly.

## Related

- [`.claude/skills/ship/SKILL.md`](../.claude/skills/ship/SKILL.md) — the workflow.
- [`release.md`](release.md) — what happens after merge, when a release ships.
- [`extensibility.md`](extensibility.md) — the rule the protocol-compatibility
  review leans on.
