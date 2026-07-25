---
type: Process
title: Shipping process
description: The evidence and safety bar every change clears before it merges — offline proof matching the risk, a contract review, green CI.
tags: [shipping, review, testing, ci]
timestamp: 2026-07-25
---

# Shipping process

## Status

Implemented.

## Abstract

Shipping means completing the requested change, gathering proof that matches the
risk it carries, opening a mergeable PR, and merging only after CI is green.

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
4. Merge only from a safe branch state with green CI.

## Ownership boundary

- This spec owns the intent, the constraints, and the bar a shipped change clears.
- The skill owns the workflow: order of operations, heuristics, and commands.

## Required outcomes

Every shipped change MUST satisfy all of these.

1. **Safe branch state.** Never ship from `main`. Working tree clean before the
   final push, and the branch up to date with `origin/main` before merge.
2. **Goal achieved with evidence.** The requested behavior is implemented, and
   the proof is the smallest thing that would convince a skeptical reviewer.
3. **Behavior covered by an offline test.** Every behavior change gets an
   automated test that drives the changed seam's real entry point — a turn
   through the executor, a fold of the event stream, a driver's wire mapping —
   not a constructor or adjacent code that still compiles. Tests stay offline;
   the `SimDriver` and the canned MCP server make that always possible. Docs- or
   config-only changes are exempt with stated justification.
4. **Contract reviewed.** The library-risk review below is performed and its
   result recorded in the PR body.
5. **Synced artifacts.** `README.md` for user-facing surface, `AGENTS.md` for
   agent guidance, `knowledge/` plus [`index.md`](index.md) when intent changes, and
   `CHANGELOG.md` under `[Unreleased]`. No code-duplicating prose.
6. **Smoke test the affected surface.** Beyond the automated test, run the flow:
   `cargo run -p agentyk --example hello` for framework changes, a live-provider
   run for anything touching the `http` drivers, the example binary itself for
   changes under `examples/`. Docs- or config-only changes may state why not.
7. **Follow-ups surfaced.** Deferred work, partial fixes, declined suggestions,
   and known drift go under **Follow-ups** in the PR body, one line of rationale
   each — or "No follow-ups."
8. **Safe merge.** The PR uses the template, CI is green, and every review
   comment is answered inline on its own thread and resolved — including nits,
   low-confidence suggestions, and bot comments, and including when the
   resolution is a pure code change. Squash-only, after a final clean comment
   sweep.

## Contract review

Mandatory for every change touching code, manifests, or CI. agentyk hosts
nothing and spawns nothing on a user's machine, so the categories are the
promises the crates make rather than a host threat model. For each one the
change touches, look for the failure in the diff:

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
- **Dependency risk.** A new crate needs a one-line justification. The two
  published crates move in lockstep — see [`release.md`](release.md).

Docs-only, comment-only, or test-only changes may record "No contract-relevant
changes" with a one-line justification.

## Constraints

- Shipping is outcome-oriented, not a linear checklist to walk in order.
- Validation starts with the smallest high-signal proof and deepens only where
  risk demands it.
- The contract review is not waived by a change looking low-risk.
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
[`release.md`](release.md). Coverage must prove the change, including the
negative paths that matter.

## Merge discipline

- Conventional Commits PR titles, under 70 characters.
- Squash and merge only.
- GitHub Actions is the source of truth. Never merge red CI.
- After merging, watch main's CI for the merge commit; a failure there is a
  shipping regression, to be fixed or reverted promptly.

## Related

- [`.claude/skills/ship/SKILL.md`](../.claude/skills/ship/SKILL.md) — the workflow.
- [`release.md`](release.md) — what happens after merge, when a release ships.
- [`extensibility.md`](extensibility.md) — the rule the protocol-compatibility
  review leans on.
