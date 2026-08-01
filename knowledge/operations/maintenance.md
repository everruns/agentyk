---
title: Knowledge Maintenance
type: Process
description: Defines the repository definition of done for maintaining persistent OKF knowledge.
status: stable
---

# Knowledge Maintenance

`knowledge/` is Agentyk's persistent repository memory and an OKF v0.2 bundle.
It records durable
product and architecture intent so future maintainers and agents can recover
why the repository works as it does without relying on chat history.

## Definition of done

A change is done only when the repository and its knowledge agree:

1. Read the relevant knowledge before changing behavior, architecture, policy,
   public capability, or maintenance workflow.
2. Integrate every durable decision introduced by the change into the relevant
   knowledge document. Add a focused document and index entry when no existing
   concept owns it.
3. Revise or remove stale and contradictory claims in the same change. Do not
   append a second truth or leave migration notes as the current contract.
4. Keep implementation details in source and user tasks in `README.md`; keep
   durable why/what, constraints, policies, and roadmaps in this bundle.
5. Update links from instructions, workflows, code comments, and public docs
   when knowledge moves or is renamed.
6. Run the strict OKF validator and the checks required by
   [Shipping](shipping.md).

A pull request may state that no knowledge update is needed only when it adds
no durable decision and leaves every existing claim accurate.

## Bundle maintenance

The authoritative format is [Open Knowledge Format v0.2][okf-spec].
Agentyk adds a stricter repository profile on top of that format:

- Keep one concept per Markdown file, as required by OKF’s concept model.
- Give every concept YAML frontmatter with `type`, `title`, and a one-sentence
  `description`; OKF itself requires only `type`.
- Declare `okf_version: "0.2"` in the root [index](index.md), group entries for
  progressive disclosure, and list every concept exactly once using its
  frontmatter description.
- Use OKF `sources` for material a concept derives from and keyed Markdown
  footnotes for per-claim attribution. Do not add a body citations list.
- Use `generated`, `verified`, `status`, and `stale_after` only when their OKF
  semantics apply; do not use the obsolete `timestamp` field.
- Prefer links to duplication; a linked concept remains the single source of
  truth.
- Review nearby concepts when changing one so relationships and terminology
  remain coherent.
- Preserve useful history in Git, not as obsolete prose in the current bundle.

## Integration points

[AGENTS.md](../../AGENTS.md) requires agents to consult and maintain this bundle.
The [ship workflow](../../.claude/skills/ship/SKILL.md) enforces knowledge review
as a required outcome, and [Shipping](shipping.md) defines the release bar.

[okf-spec]: https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md
