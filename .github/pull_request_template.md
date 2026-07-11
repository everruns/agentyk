## What changed
Describe the change functionally — what behavior changes and its impact on
callers. Lead with outcomes; don't walk through code locations, the diff shows
where and how. Keep any code-level notes short and specific.

## Why
Problem or motivation.

## Before / After
Show the effect with evidence whenever behavior changes — CLI/API output, logs,
or test results. For changes with no observable behavior (pure refactor, docs),
say so.

## Checklist
- [ ] Tests added or updated (offline, via `SimDriver` — no keys/network)
- [ ] `cargo fmt --all --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- [ ] `agentyk-core` still free of tokio/reqwest in its normal dep tree (`cargo tree -p agentyk-core --edges normal`)
- [ ] Specs updated if behavior changed
