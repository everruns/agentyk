# Extending agentyk without changing core

Agentyk-core is a **contract crate**: traits, the event protocol, and the
sans-IO turn machine. The goal is that a downstream project — the eventual
everruns-core rebuild, a `agentyk-everruns` compat layer, or any adopter —
can reproduce everruns-grade behavior **as a library composed over agentyk's
seams**, without patches to core. This note is the map for doing that: what is
already external, the one thing that genuinely needs core, and the rule we use
to decide.

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
  [`EventData::Custom`]). The satellite owns the schema; core never grows a
  field for it.
- **Behavior → a satellite [`TurnExecutor`] + capabilities.** Never core.

## What already composes over existing seams (no core change)

| everruns capability | Build it with |
| --- | --- |
| Guardrails that **mutate/redact** a call, **approval** pauses, **parallel** tool dispatch, capability-contributed hooks | A custom [`TurnExecutor`] — it owns the whole reason/act loop over `atoms` + [`TurnState`]; `InProcessExecutor` is just one policy |
| MCP server merging (`mcp_servers()`) | A `Capability` whose `tools()` connects and returns the servers' tools (see the bundled `McpCapability`) |
| Tool risk taxonomy / `ToolHints` | The satellite's tool wrapper + its executor's approval step; hints are host-side, never sent to the model — carry them in `ToolDefinition.metadata` |
| Narration, `status()`/`category()`/`icon()`, `facts()`, richer command results | Capabilities + `EventListener`s (narration is a listener over the event stream); `Capability::metadata()` for status/category |
| Compaction / infinity-context | A real `ContextAssembler` + `EventData::Custom` for `context.compacting`/`context.compacted` |
| File-system depth (mounts, grep, stat, edit) | More `FileSystem` tools + store impls behind the `fs` feature |
| Host services reaching tools | `ToolContext.extensions` (typed, `TypeId`-keyed bag) |
| A domain event core lacks | `EventData::Custom { event_type, payload }` |

The **[`TurnExecutor`] seam is the lever** that makes hooks/approval/parallel
external. Because the machine is sans-IO — pure [`TurnState`] transitions plus
stateless `atoms` — a satellite `EverrunsExecutor` can run its own act loop:
consult capability-contributed hooks, mutate or deny a call, await an approver,
fan out the batch concurrently ([`TurnState::pending_tool_actions`] returns the
whole not-started batch for exactly this), and record whatever events it wants
via `TurnHost::record`. It does **not** have to use agentyk's built-in
`PreToolUseHook`. That is the intended resolution of the "mutating /
capability-contributed guardrails" gap: it lives in the executor layer, not in
core's hook trait.

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
   fields land in the bag, and the satellite interprets them.

All are additive and serde-optional (`skip_serializing_if`, `#[serde(default)]`),
so a plain message/tool/event serializes exactly as before and pre-0.1.1 logs
still deserialize.

## The satellite boundary

> `agentyk-everruns` (or the rebuilt everruns-core) =
> a custom [`TurnExecutor`] (everruns act/hook/approval/parallel semantics)
> + capabilities (mcp-merge, narration listener, compaction assembler, facts,
> filesystem depth)
> + drivers
> + `metadata` conventions (a documented schema for what rides in the hatches).

agentyk-core stays frozen and lean; the satellite owns everruns' shapes in its
own layer. When core *does* need to change, the test is the rule above: is the
new thing universal, correctness-load-bearing protocol data? If not, it belongs
in a `metadata` hatch or the executor, not in core.

[`EventData::Custom`]: https://docs.rs/agentyk-core/latest/agentyk_core/event/enum.EventData.html
[`TurnExecutor`]: https://docs.rs/agentyk-core/latest/agentyk_core/executor/trait.TurnExecutor.html
[`TurnState`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
[`TurnState::current_message_id`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
[`TurnState::pending_tool_actions`]: https://docs.rs/agentyk-core/latest/agentyk_core/turn/struct.TurnState.html
