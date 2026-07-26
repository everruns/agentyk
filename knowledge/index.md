# Agentyk Knowledge

Durable design intent, constraints, and contracts for maintainers — the **why**
and **what** behind agentyk, not exhaustive **how** (link to source for exact
fields, enum variants, and API shapes rather than duplicating them). Read the
relevant knowledge before changing behavior in that area and keep it current as part of the same change.

This directory is an [Open Knowledge Format](https://okf.md) v0.1 bundle: each
concept is one markdown file carrying a `type` in its YAML frontmatter, and this
`index.md` is the bundle listing (read first for progressive disclosure).
Public, task-oriented documentation for users lives in the top-level `README.md`,
not here — nothing in this bundle is a user-facing product doc.

## Concepts

- [architecture](architecture.md) (`Design`) — crate boundaries, the
  single canonical step engine, execution hosts, and event-sourced durability.
- [diagrams](diagrams.md) (`Design`) — Mermaid/SVG source pairing, placement,
  visual language, accessible color use, and required raster review.
- [maintenance](maintenance.md) (`Process`) — the definition of done for keeping persistent repository memory current and internally coherent.
- [plan](plan.md) (`Plan`) — direction and the phased roadmap: build agentyk
  value-first, harden for adoption, then rebuild everruns-core/runtime on top.
- [everruns-adoption](everruns-adoption.md) (`Analysis`) — gap analysis of what
  agentyk still lacks before everruns-core/runtime can re-base onto it, tiered by
  where each gap must land (core / framework / everruns layer).
- [yolop-adoption](yolop-adoption.md) (`Analysis`) — gaps found by actually
  porting yolop's execution story onto agentyk's seams, ordered by how hard
  each blocks a shipping coding agent.
- [extensibility](extensibility.md) (`Design`) — the rule for what earns a
  first-class core field versus a generic `metadata` hatch: behavior is external,
  data extensibility is core.
- [release](release.md) (`Process`) — the CI-driven publishing flow: a
  `chore(release)` commit tags, releases, and publishes `agentyk-core`,
  `agentyk-engine`, then `agentyk` to crates.io.
- [session-timelines](session-timelines.md) (`Design`) — immutable historical
  points, bounded reads, branch lineage, disposable snapshots, and context
  assembly over long-lived event streams.
- [shipping](shipping.md) (`Process`) — the bar a change clears before it
  merges: offline proof matching the risk, a contract review over the promises
  the crates make, green CI. Adopted from yolop and reduced to a library's
  shape.
