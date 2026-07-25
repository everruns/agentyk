---
type: Design
title: agentyk architecture
description: Target crate boundaries, canonical step engine, host execution model, and event-sourced durability.
tags: [architecture, engine, durability, events, crates]
timestamp: 2026-07-25
---

# Agentyk architecture

Status: Implemented.

## Intent

Agentyk has one portable agent protocol and one canonical implementation of
turn semantics. Local execution, Yolop, and a durable Everruns deployment are
different hosts of that engine, not independent turn-loop implementations.

The public API remains value-first: applications compose an `Agent` from
objects and run sessions without first creating or registering entities.

Public orientation is in [`../docs/architecture.md`](../docs/architecture.md).
This spec records the architectural constraints maintainers must preserve.

## Crate boundaries

The workspace has three library layers.

### `agentyk-core`

Core owns portable contracts and deterministic domain behavior:

- serializable messages, events, model/tool values, and correlation ids;
- extension traits such as `Tool`, `Capability`, and `ChatDriver`;
- the event-store contract;
- serializable turn state and pure reducers from durable events.

Core does not own the orchestration loop or concrete infrastructure. It must
remain free of Tokio, HTTP, process spawning, and filesystem access.

### `agentyk-engine`

Engine owns the canonical interpretation of a turn:

- `Agent`, its builder, and `Session`;
- agent and capability assembly;
- context preparation;
- middleware, cancellation, budget, and failure semantics;
- preparation of one operation and application of its result;
- the in-process runner.

There is one engine. Durable execution does not introduce another engine or a
custom `TurnExecutor` that copies the loop. Hosts provide operation execution,
scheduling, and persistence.

### `agentyk`

The top-level crate is the application facade. It explicitly re-exports core
and engine and owns bundled implementations:

- simulation, OpenAI-compatible, and Anthropic drivers;
- in-memory and JSONL event stores;
- MCP support;
- filesystem support.

MCP and filesystem are first-class library modules and capabilities, not
integration crates. Other integrations also remain feature-gated modules in
`agentyk` for now. Crate extraction is not an architectural growth policy; it
requires a concrete dependency, release, or ownership need.

## Definition versus environment

The implementation separates behavioral definition from runtime environment:

- `AgentDefinition`: name, instructions, default model, capabilities, and turn
  policy;
- `AgentEnvironment`: drivers, observers, and runtime services.
- `Session` / `TurnHost`: the event store and other run-specific resources.

The facade builder may populate definition and environment and return one
`Agent`; a session supplies its event store separately. The split exists so a
durable host can reconstruct host resources independently of behavioral
configuration, not to introduce an identity-first lifecycle.

`ModelSpec` currently includes optional credentials and endpoint overrides, so
an `AgentDefinition` containing them is sensitive configuration rather than a
freely serializable artifact. Prepared operations are runtime values, not
event payloads: hosts must not persist or log them. Durable events contain no
model credentials, and a durable activity reconstructs execution resources
from its protected host configuration.

Invalid definitions are not public states. In particular, a built
`AgentDefinition` always has a model.

## Step engine

The whole-loop `TurnExecutor` is replaced by a canonical step protocol. At a
conceptual level the engine:

1. reduces durable events into `TurnState`;
2. prepares the next operation and its transition events;
3. accepts the operation result;
4. returns the events produced by that result.

Operations cover model invocation, a batch of tool invocations, and terminal
completion. The exact Rust types belong in source rather than this spec.

The engine, not a host, owns:

- middleware order and rewrite/deny semantics;
- cancellation and budget decisions;
- context assembly;
- error and outcome mapping;
- domain-event generation;
- state transitions.

A host owns:

- immediate execution versus durable scheduling;
- sequential versus concurrent dispatch of a prepared tool batch;
- retries, queues, leases, and workers;
- the concrete event-store transaction;
- tenant and server infrastructure.

The built-in in-process runner repeatedly executes steps in one async call. A
durable Everruns host executes the same protocol one persisted activity at a
time. Behavioral equivalence follows from sharing the engine rather than from
tests comparing two loop implementations.

## Event authority

Durable events are the authoritative persistence seam. Replay must reconstruct
both message history and actionable turn state, including data that changes
what will execute, such as rewritten tool arguments.

Snapshots are permitted only as disposable replay optimizations:

```text
reduce(snapshot, events_after_snapshot) == reduce(all_events)
```

The event-store contract must support:

- atomic append of a batch;
- expected-version concurrency control;
- ordered incremental reads.

The engine advances live projections only after an append succeeds. A failed
append must not leave in-memory history ahead of durable history.

Ephemeral notifications, including streaming deltas, go to observers and do
not enter the durable store. Forward compatibility distinguishes events that
are merely observational from events required by reducers: an unknown
state-bearing event makes replay unsupported, while an unknown observational
event may be ignored or exposed as custom data.

## Capability contribution

A capability remains attached by object. Its resolved contribution may group:

- system instructions;
- tools;
- middleware governing those tools.

This is the mechanism for first-class library capabilities such as filesystem
and MCP. It avoids requiring the host to attach a tool and independently know
which policy belongs with it.

Host-facing commands and presentation metadata may be exposed by higher
layers, but must not make the core turn reducer depend on a UI or server.

## Module direction

Physical modules and public module paths should express the same domains.
Generic source buckets whose contents are re-exported into unrelated public
paths are not the target layout.

Dependencies point inward:

```text
agentyk bundled modules -> agentyk-engine -> agentyk-core
host infrastructure ----> engine/core contracts
```

Core never depends on engine or bundled modules. Engine never depends on a
provider driver, MCP transport, filesystem implementation, or host
infrastructure.

## Migration order

The architecture landed in this order:

1. establish an event-store contract with atomic batches and stream versions;
2. make complete `TurnState` replay from durable events possible;
3. introduce the canonical prepare/apply step engine;
4. reduce in-process execution and the Everruns POC to hosts/dispatchers;
5. split agent definition from runtime environment;
6. introduce `agentyk-engine` and then align physical modules with public
   domains.

New behavior must not reintroduce a whole-loop executor extension point or a
serialized turn checkpoint as a second source of truth.
