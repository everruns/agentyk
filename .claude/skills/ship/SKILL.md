---
name: ship
description: Goal-oriented workflow for landing a change to agentyk safely. Use when the user asks to ship, fix and ship, take a change through validation, or drive a PR through CI to merge.
---

# Ship

Goal: land the requested change with evidence, and merge only after CI is green.

Read [`knowledge/shipping.md`](../../../knowledge/shipping.md) § Required outcomes first —
it owns the bar. This skill owns how to reach it.

"Fix and ship" means implement first, then switch into shipping mode.

## Working the change

Start from the goal and the risk the diff actually changes, not from checklist
order. Review the delta (`git diff origin/main...HEAD`, `git log origin/main..HEAD`),
confirm the requested behavior is really implemented, then pick the smallest
evidence a skeptical reviewer would accept: targeted diff reading, a focused
test, then the [checks in `AGENTS.md`](../../../AGENTS.md) for the surfaces you
touched.

Two kinds of evidence, neither substituting for the other:

- **The test.** It drives the changed seam's real entry point — run a turn
  through the executor, fold the event stream, exercise the driver's wire
  mapping — not a constructor. It stays offline: the scripted `SimDriver` and
  the canned stdio MCP server mean a missing API key is never the reason a
  behavior went untested.
- **The smoke test.** Run the affected flow. `cargo run -p agentyk --example
  hello` for framework changes; the example binary for anything under
  `examples/`; a live-provider run for the `http` drivers, since their wire
  format is the one thing `SimDriver` cannot prove.

Before pushing, reread the diff for duplication and accidental complexity you
introduced, and fix it.

Stop and report only for blockers you cannot resolve alone: a merge conflict you
cannot judge, missing credentials, ambiguous intent, a CI failure you cannot
reproduce.

## Contract review

Mandatory for every change touching code, manifests, or CI — a change looking
low-risk does not excuse it. The categories and their checks are in
[`knowledge/shipping.md`](../../../knowledge/shipping.md) § Contract review: core purity,
feature gates, protocol compatibility, host neutrality, secrets, dependency
risk. Run the two CI steps that are wider than the local defaults before you
push, because both fail the build:

```sh
cargo clippy -p agentyk --no-default-features --lib --tests -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --all-features
```

Record the result in the PR body. Docs-, comment-, or test-only changes may say
"No contract-relevant changes" with a one-line justification.

## PR and merge

Write the body around functional change and impact — what changed, why, how it
was validated, what it risks — using
[`.github/pull_request_template.md`](../../../.github/pull_request_template.md).
Two sections are never omitted: the contract review above, and **Follow-ups**
(everything deferred, one line of rationale each, or "No follow-ups."). Default
to doing in-scope work rather than deferring it.

Say explicitly whether knowledge changed: either which concept you updated in
`knowledge/` (plus its [`index.md`](../../../knowledge/index.md) entry), or
that no knowledge update was required and why. Follow
[`knowledge/maintenance.md`](../../../knowledge/maintenance.md): integrate durable
decisions and stale-claim cleanup in the same change.

Answer every review comment inline on its own thread and resolve it — nits,
low-confidence suggestions, and bot comments included, and a written reply is
required even when the fix was a pure code change.

Merge squash-only once CI is green and a final comment sweep is clean, giving
async reviewer bots a couple of minutes after CI turns green. Do not enable
auto-merge — bots can post after the last push. Then watch main's CI for the
merge commit, and fix or revert promptly if it fails.
