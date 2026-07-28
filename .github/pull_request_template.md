## What changed
Describe the change functionally — what behavior changes and its impact on
callers. Lead with outcomes; don't walk through code locations, the diff shows
where and how. Keep any code-level notes short and specific.

## Why
Problem or motivation.

## Before / After
Show the effect with evidence whenever behavior changes — CLI/API output, logs,
screenshots or VHS recordings, and test results. A fix must reproduce the
failure before the change and prove it corrected afterward. If the regression
test is the complete reproduction, say why another artifact adds nothing. For
changes with no observable behavior (pure refactor, docs), say so.

## Impact and public API
Name affected callers, crates, features, persisted data, and operational paths.
List public items added, changed, or removed; compatibility and migration
consequences; and alternatives considered when the API shape is not obvious.
For no public-API impact, say why.

## Security
List relevant input and trust boundaries reviewed, findings, and mitigations.
For no security-relevant changes, say why.

## Performance
List hot paths or resource bounds reviewed and any measurements. For no
performance-relevant changes, say why.

## Contract review
Record the applicable core-purity, feature-gate, protocol-compatibility,
host-neutrality, secrets, and dependency-risk findings. For no
contract-relevant changes, say why.

## Documentation and specs
List rustdoc/examples, README, demos or recordings, knowledge concepts, and
changelog entries updated. For each inapplicable artifact, say why.

## Follow-ups
List deferred work, partial fixes, declined suggestions, and known drift with a
one-line rationale each, or write "No follow-ups."

## Checklist
- [ ] Reproduction and before/after evidence captured when fixing a bug
- [ ] Meaningful success, failure, boundary, regression, and integration paths covered
- [ ] Affected flow smoke-tested
- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `agentyk-core` still free of tokio/reqwest in its normal dep tree (`cargo tree -p agentyk-core --edges normal`)
- [ ] Public API, security, performance, and contract reviews recorded above
- [ ] Public docs, demos, knowledge, and changelog updated or justified above
- [ ] Branch contains current `origin/main`; latest main CI and every PR check are green
