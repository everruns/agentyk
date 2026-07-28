---
type: Design
title: Multi-actor sessions
description: >-
  Defines the engine and host boundary for shared sessions with multiple users
  and agents.
tags: [sessions, actors, participants, events, everruns]
---

# Multi-actor sessions

## Status

Implemented at the portable protocol and engine boundary. Participant storage
and policy remain host concerns.

## Model

A session is one shared event timeline, not one agent identity. Its host agent
is the default responder, but an embedding host may resolve an addressed
participant to another by-value `Agent` and execute that turn through
`Session::run_with_agent`. Every responder reads and appends the same replayable
message history.

The session does not own a participant registry. Everruns needs durable
principal, membership, role, join/leave, and authorization records; a standalone
library does not. Pulling those records into core would violate the value-first,
host-neutral model. Core instead exposes the data needed for a host to preserve
that model:

- `Message.external_actor` identifies users arriving through external channels.
  Model-facing copies prefix their text with the actor's display label, while
  the durable message remains unchanged.
- `Event.metadata` preserves host-owned provenance such as `participant_id`,
  `agent_id`, or principal information without assigning a schema to it.
- `Session::run_with_agent` and its options/recovery forms select behavior for
  one turn without changing the session's default host.

## Invariants

- Unaddressed `Session::run` calls always use the host agent.
- Addressing, membership validation, roles, leave state, and authorization are
  resolved before the engine call by the embedding host.
- The selected agent contributes its behavioral definition and model defaults
  for that turn. The host keeps the execution environment (drivers, listeners,
  and extensions) together with the session id, event store, history, and
  steering queue.
- Agent configuration is not persisted in the event log. Recovering an
  incomplete addressed turn therefore requires supplying the same agent value
  through `resume_pending_with_agent`.
- External-actor labels alter only provider context. Replay and inspection
  return the original message content and identity value.
- Event metadata is observational. Reducers do not depend on a host's
  participant schema.

## Everruns adoption boundary

Everruns retains participant rows, `host`/`member` policy, principal
provenance, invite-mode handoff, and addressed-participant resolution. Its
event-store adapter may enrich `EventRequest.metadata` before persistence.
After resolution, it passes the selected agent value to the canonical engine;
it does not need a separate multi-actor turn loop.

The user-facing [`examples/osbb`](../examples/osbb) application is the
acceptance example for multiple named external users sharing one session. Its
offline `SimDriver` test proves raw-versus-provider context behavior, while its
recorded live-provider flow exercises the same contract against
`gpt-5.6-terra`.
