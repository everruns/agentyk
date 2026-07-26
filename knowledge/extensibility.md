---
type: Design
title: Extending agentyk without changing core
description: The rule for what earns a first-class core field versus a metadata hatch — behavior is external, data extensibility is core.
tags: [extensibility, core, metadata, design]
timestamp: 2026-07-24
---

# Extending agentyk without changing core

Agentyk-core is a **contract crate**: traits, the event protocol, and pure turn
reducers. The goal is that a downstream project — the eventual
everruns-core rebuild, a `agentyk-everruns-poc` compat layer, or any adopter —
can reproduce everruns-grade behavior **as a library composed over agentyk's
seams**, without patches to core. This note is the map for doing that: what is
already external, the one thing that genuinely needs core, and the rule we use
to decide. The crate and engine boundaries are defined in
[`architecture.md`](architecture.md).

## The rule: behavior is external, data extensibility is core

You cannot add a field to another crate's serialized `struct`/`enum`. So the
only thing an external crate fundamentally *can't* do is extend the protocol
types that flow through the driver, the event log, and replay. Everything
else — orchestration, hooks, capabilities, narration, compaction — is
behavior, and agentyk already exposes seams for behavior.

Therefore:

- **Universal, correctness-load-bearing data → first-class typed fields in
  core.** Things every adopter needs and that must survive
  serialize → replay → driver round-trip, and are *not* everruns-specific.
- **Everruns-flavored richness → generic `metadata` hatches.** An opaque,
  serializable bag on the protocol types (the data analogue of
  [`EventData::Custom`]). The adopting host owns the schema; core never grows a
  field for it.
- **Behavior → engine extension points, capabilities, and host dispatchers.**
  Never duplicated whole-turn executors.

## Where a new knob goes

Composition — anything an agent *is* — belongs to `AgentDefinition`. Shared
runtime dependencies such as drivers, observers, and services belong to
`AgentEnvironment`; the event store is supplied per `Session` / `TurnHost`.
`AgentBuilder` may configure definition and environment and return one
runnable value, so this internal boundary does not add a registry or an
identity-first lifecycle. See
[`architecture.md`](architecture.md#definition-versus-environment).

## What already composes over existing seams (no core change)

| everruns capability | Build it with |
| --- | --- |
| Guardrails that **mutate/redact** a call, **approval** pauses, capability-contributed hooks | `middleware::TurnMiddleware` — `before_tool` returns `Proceed`/`Rewrite`/`Deny`, `after_tool` transforms the result; attached with `AgentBuilder::middleware` |
| **Parallel** tool dispatch | A host tool dispatcher over the batch prepared by the canonical step engine; it never owns the whole turn loop |
| MCP server merging (`mcp_servers()`) | A `Capability` whose `tools()` connects and returns the servers' tools (see the bundled `McpCapability`) |
| Tool risk taxonomy / `ToolHints` | A tool wrapper + engine middleware; hints are host-side, never sent to the model — carry them in `ToolDefinition.metadata` |
| Narration, `status()`/`category()`/`icon()`, `facts()`, richer command results | Capabilities + `EventListener`s (narration is a listener over the event stream); `Capability::metadata()` for status/category |
| Compaction / infinity-context | A real `ContextAssembler` + `EventData::Custom` for `context.compacting`/`context.compacted` |
| File-system depth (mounts, grep, stat, edit) | More `FileSystem` tools + store impls behind the `fs` feature |
| Host services reaching tools | `ToolContext.extensions` (typed, `TypeId`-keyed bag) |
| A domain event core lacks | `EventData::Custom { event_type, payload }` |
| Model capability knowledge (context window, supported efforts) | `profile::ModelCatalog` — a host-implemented seam, because a model list inside a library is stale by the next provider release |
| Remote-service credentials that expire | An auth provider trait asked **per request**, not a config field read once — see `mcp::McpAuthProvider` |

`Custom` also provides forward compatibility for **observational** events. An
unknown kind can be retained as custom data so a listener or newer host may
inspect it. This tolerance must not make an older engine claim it can safely
resume through an event required to reduce turn state. Unknown
correctness-bearing events stop replay; unknown observational events may be
ignored. See [`architecture.md`](architecture.md#event-authority).

Two seams have a deliberate division of labour. **Middleware** owns policy
about a call — deny it, rewrite it, transform its result — and is orchestrated
by the canonical engine. **A host dispatcher** owns how a prepared operation
runs, including sequential or concurrent execution of a tool batch. It cannot
change middleware order, transition semantics, or event meanings.

Getting that division wrong is expensive, and we got it wrong once: because
core's hooks could only allow or deny, a satellite that merely wanted to redact
an argument had to fork the entire act loop, duplicating the cancel check, the
delta sink, and the outcome mapping — code that then drifts. The canonical step
engine removes the copied loop itself.

## What needed a core change (0.1.2)

Two more hatches, both from the yolop port
([`yolop-adoption.md`](yolop-adoption.md)), both following the rule rather
than bending it:

- **`ToolOutput.metadata`** — structured result data for the host. A tool
  result is one string because that is what every provider's wire format
  accepts; that is lossy for a UI, which then re-parses prose to render a diff
  or an exit code. The structured form is host-owned, so it is a hatch, and it
  rides on `tool.completed` so listeners and replay both see it.
- **`ModelSpec.metadata`** — provider-flavored configuration (OAuth refresh
  token, account id, organization id, gateway headers). Only the driver that
  understands a given provider reads it, which is the definition of
  everruns-flavored richness. Treated as sensitive: redacted in `Debug`
  alongside `api_key`, and a `ModelSpec` still never reaches an event.

`EventData::ToolProgress` is the counter-example worth noting — it is a typed
variant, not `Custom`, because ephemerality is a **protocol** property. The
recorder must know not to persist it, and "must not be written to the log" is
not something a host can express through an opaque payload.

`ToolOutput.parts` is the other side of the same coin: it is typed because it
is what the *model* receives, so it must round-trip through the event log and
reach a driver — the same test `Message.thinking` passed. Its host-facing
sibling `ToolOutput.metadata` is a hatch. One tool result, two audiences, and
the rule sorts them without a judgement call.

## What needed a core change (0.1.1)

Three protocol types must physically carry data an external crate can't add,
because they cross the driver / event-log / replay boundary:

1. **`Message.thinking` + `Message.thinking_signature`** — typed. Provider
   reasoning ("extended thinking") must round-trip back to the provider on
   later turns for the exchange to stay valid, and the driver only ever sees a
   `Message` — there is no side channel. Universal to modern reasoning models
   (OpenAI, Anthropic), not everruns-specific, so it earns typed fields.
2. **`message_id` on `output.message.{started,delta,completed}`** — typed. A
   stable id correlating the three events of one streaming assistant message.
   Universal streaming concern; carried on [`TurnState::current_message_id`]
   for the reason step and stamped onto all three events.
3. **Generic `metadata` hatches** — `Message.metadata`,
   `ToolDefinition.metadata`, and `Capability::metadata()`. This is where
   everruns-flavored richness rides: execution `phase`, narration hints, the
   tool risk/hint taxonomy, capability status/category/icon. Adding these once
   means core does **not** need another change as everruns evolves — new
   fields land in the bag, and the adopting host interprets them.

All are additive and serde-optional (`skip_serializing_if`, `#[serde(default)]`),
so a plain message/tool/event serializes exactly as before and pre-0.1.1 logs
still deserialize.

## The adopter boundary

> Rebuilt everruns-core =
> a durable host and operation dispatchers
> + capabilities (narration, compaction, facts, filesystem depth)
> + drivers
> + `metadata` conventions (a documented schema for what rides in the hatches).

Agentyk's engine owns turn semantics; Everruns owns durable scheduling and its
host-specific shapes. When core *does* need to change, the test is the rule
above: is the new thing universal, correctness-load-bearing protocol data? If
not, it belongs in a metadata hatch, capability, middleware, or host.

## Proven end-to-end

`poc/agentyk-everruns-poc` is a working proof of the extension surface (a
proof of concept, `publish = false`). Its library depends on `agentyk-core`
only; the facade appears as a dev dependency for end-to-end tests. It ships:

- `ApprovalMiddleware` — hint-based approval as ordinary core middleware:
  **deny with a user-facing message**, composed with a redaction middleware
  that **rewrites a call before it runs**, plus **capability-contributed
  guards** (a satellite capability bundles a tool and the middleware governing
  it). All three shapes of gap 4, none of them needing a forked act loop.
- `ToolHints` (`readonly`/`destructive`/`open_world`) carried in
  `ToolDefinition.metadata` under a `"hints"` key — the metadata hatch driving
  real behavior, with core none the wiser.
- `NarrationListener` — an `EventListener` that renders the event stream into
  transcript lines, showing everruns' largest UI surface (`tool_narration`) is
  a pure observer, not a turn-loop concern.
- `MemoryAssembler` — a `ContextAssembler` that injects a persistent memory
  note into every turn and can cap replayed history (`keep_last`), showing
  everruns-style **memory + compaction** shape *what the turn sends* over the
  existing context-assembly seam, while the untrimmed history stays in the log.

Its tests drive a real agent through the canonical engine and assert a
destructive tool is blocked with the approver's message, an approved one runs,
a readonly one bypasses approval, a two-tool batch is fanned out and gated
per-call, a guard **redacts** a secret argument before the tool sees it, guards
**compose** (first-deny-wins), a capability **contributes** the guard that gates
its own tool, the narration reads back as a transcript, and a `MemoryAssembler`
**injects a memory note and compacts history** in what the driver receives while
the log stays whole.

[`EventData::Custom`]: https://docs.rs/agentyk-core/latest/agentyk_core/event/enum.EventData.html
[`TurnState`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
[`TurnState::current_message_id`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
[`TurnState::pending_tool_actions`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
