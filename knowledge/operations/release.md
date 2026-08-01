---
type: Process
title: Release process
description: Defines lockstep versioning, crates.io publication, and release verification.
tags: [release, ci, crates-io, publishing]
---

# Release process

## Status

Implemented.

## Abstract

Releases are CI-driven. An agent (or human) prepares a release PR that bumps
the version and updates the changelog; on merge to `main`, GitHub Actions
creates the GitHub Release + tag and publishes all four crates to crates.io. No
crates.io token ever lives on a developer machine — it lives only in the
repository's Actions secrets.

Modeled on `everruns/bashkit`'s release process, reduced to agentyk's shape:
four crates, one registry (crates.io), no git-only dependencies to strip.

## Versioning

Pre-1.0. Agentyk stays **strictly on `0.1.x`** until a deliberate public-launch
decision — every release bumps the patch component (`0.1.z`). Do not bump to
`0.2` or `1.0` without that decision. Breaking internal API changes are allowed
within `0.1.x` (agentyk is pre-adoption).

## Crates and publish order

All four library crates share one workspace version (`Cargo.toml`'s
`[workspace.package]` version). They publish in dependency order:

1. `agentyk-core` — the contract crate, no internal deps.
2. `agentyk-engine` — the canonical engine, depends on `agentyk-core`.
3. `agentyk-macros` — proc-macro implementation, no internal deps.
4. `agentyk` — the facade crate, depends on core, engine, and macros (each a
   path-and-version dependency; cargo resolves the registry version at publish
   time).

## Release workflow

Flow: **prepare → verify → merge → monitor**. Skipping verify risks tagging a
release that fails to publish; skipping monitor risks declaring "shipped" while
crates.io silently failed.

### Human steps

1. Ask the agent to prepare a release ("Create release v0.1.1").
2. Review the PR, including the publish-readiness report.
3. Merge to `main` — CI creates the Release and publishes.
4. Ask the agent to monitor until crates.io shows the new version.

### Agent steps (prepare)

1. **Ensure full git history** — cloud sandboxes are often shallow-cloned,
   hiding commits and yielding a wrong changelog. Run
   `git fetch --unshallow origin main 2>/dev/null || git fetch origin main`.
2. **Determine version** — human-specified, or the next `0.1.z` patch.
3. **Update the version** in the workspace `Cargo.toml` `[workspace.package]`
   (all four library crates inherit it via `version.workspace = true`). Keep the
   `[workspace.dependencies]` requirements for `agentyk-core` and
   `agentyk-engine` and `agentyk-macros` in sync with the new version. Refresh
   `Cargo.lock` with a plain `cargo build`.
4. **Update `CHANGELOG.md`** — add a `## [X.Y.Z] - YYYY-MM-DD` section (format
   below). `release.yml` extracts the notes by matching this exact header.
5. **Local verification** — `cargo fmt --all --check`,
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace --all-features`, and confirm `agentyk-core` is still
   free of tokio/reqwest in its normal dep tree
   (`cargo tree -p agentyk-core --edges normal`).
6. **Verify publish-readiness** (catches what tests don't — packaging, missing
   files, version drift):
   - `cargo publish -p agentyk-core --dry-run` must succeed.
   - `cargo publish -p agentyk-macros --dry-run` must succeed.
   - `cargo publish -p agentyk-engine --dry-run` and
     `cargo publish -p agentyk --dry-run` resolve their internal dependencies
     from the registry. On the first release of this four-crate layout they
     may not package locally until the preceding crate exists. This is
     expected: `publish.yml` publishes in dependency order and waits for each
     index update.
   - Version sync: the workspace version is greater than the latest published
     version on crates.io (`cargo search agentyk-core`,
     `cargo search agentyk-engine`, `cargo search agentyk-macros`,
     `cargo search agentyk`).
7. **Commit and push** — `chore(release): prepare vX.Y.Z` on a feature branch.
8. **Open a PR** — same title; changelog excerpt + publish-readiness report in
   the description.

### CI automation

- **`release.yml`** (trigger: push to `main` whose head commit starts
  `chore(release): prepare v`, or manual dispatch from `main`) — extracts the
  version, verifies it matches `Cargo.toml`, verifies the commit is reachable
  from `origin/main`, extracts notes from `CHANGELOG.md`, creates the GitHub
  Release + tag, then dispatches `publish.yml` against the verified tag.
- **`publish.yml`** (trigger: Release published, or manual dispatch) —
  publishes `agentyk-core`, `agentyk-engine`, `agentyk-macros`, then `agentyk`,
  waiting for the index between them. Each reads `CARGO_REGISTRY_TOKEN` from
  the `release` environment. A final job verifies all four crates report the
  new version.
  The checkout
  keeps its git credentials (this is a **private** repo — the "source is on
  main" check does an authenticated `git fetch`, which a credential-stripped
  checkout can't do). Manual dispatch may run from a release tag (version must
  match the tag) or from `main` (publishes whatever version `Cargo.toml`
  carries — the recovery path if a tag-driven publish fails after the tag was
  already cut).

## Authentication (one-time repository setup)

- Add `CARGO_REGISTRY_TOKEN` (a crates.io API token with publish scope for all
  four crates) to the repository's GitHub Actions secrets.
- Create a GitHub **environment** named `release` (Settings → Environments).
  Every publish job runs in it; add required reviewers there if you want a
  manual approval gate before anything reaches crates.io.
- The first-ever publish must be done by an owner of the crate names on
  crates.io (crates.io auto-owns a name on first publish); after that the token
  suffices.

## Pre-release checklist

CI green on `main`; `CHANGELOG.md` has a section for the new version; the
workspace version is consistent and greater than the latest published; every
available `cargo publish --dry-run` succeeds.

## Post-merge monitoring

- GitHub Release (`release.yml`): `gh release view vX.Y.Z`
- crates.io (`publish.yml`): search `agentyk-core`, `agentyk-engine`,
  `agentyk-macros`, and `agentyk`

If a workflow fails: `gh run view <run-id> --log-failed`, fix the root cause,
re-run (transient) or open a hotfix patch release (packaging/code bug). Do not
leave a release half-shipped (one crate live, the other not).

## Changelog format

- `## [X.Y.Z] - YYYY-MM-DD` header (the exact shape `release.yml` matches).
- `### Highlights` — a few impactful, user-facing bullets.
- End notable releases with `**Full Changelog**: <compare URL>`.

## Rollback

`cargo yank --version X.Y.Z agentyk` (and its core/engine/macro siblings) — use
sparingly; yanked versions still resolve for existing `Cargo.lock` files but
are not selected for new dependency resolution.
