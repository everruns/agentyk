---
okf_version: "0.2"
---

# Agentyk Knowledge

This bundle is Agentyk’s persistent repository memory: durable product and
architecture intent for maintainers and agents. It follows the authoritative
[Open Knowledge Format v0.2 specification](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md).
Read the relevant concepts before changing behavior, and maintain them as part
of the same change.

## Foundation

- [agentyk plan](plan.md) — Defines Agentyk’s product direction, vocabulary, invariants, and phased roadmap.
- [agentyk architecture](architecture.md) — Defines Agentyk’s crate boundaries, dependency direction, and runtime architecture.
- [Extending agentyk without changing core](extensibility.md) — Defines composition seams for capabilities, middleware, providers, persistence, and hosts.
- [Session timelines](session-timelines.md) — Defines immutable history, branching, snapshots, and context assembly for long-lived sessions.

## Adoption

- [Everruns adoption — gap analysis](everruns-adoption.md) — Tracks migration of everruns-core and everruns-runtime capabilities into Agentyk.
- [Yolop adoption — what a real coding agent hits](yolop-adoption.md) — Tracks reusable Yolop capabilities and the order in which Agentyk should adopt them.

## Repository Processes

- [Knowledge Maintenance](maintenance.md) — Defines the repository definition of done for maintaining persistent OKF knowledge.
- [Shipping process](shipping.md) — Defines the evidence and quality bar required before a repository change ships.
- [Release process](release.md) — Defines lockstep versioning, crates.io publication, and release verification.
- [Diagram specification](diagrams.md) — Defines how technical diagrams are authored, reviewed, and maintained.
