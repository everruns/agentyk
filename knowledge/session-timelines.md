---
type: Design
title: Session timelines
description: >-
  Defines immutable history, branching, snapshots, and context assembly for
  long-lived sessions.
tags:
  - sessions
  - events
  - replay
  - forks
  - snapshots
  - context
---

# Session timelines

## Status

Implemented.

## Model

A `Session` is one writable branch of an immutable event timeline.
`SessionPoint { session_id, sequence }` addresses an inclusive durable prefix.
Points do not move and may be inspected without executing anything.

Forking a point creates a new session value. The parent is never truncated or
rewritten, and the child records `session.forked` with its origin. The default
event-store implementation materializes the inherited prefix; a database store
may retain copy-on-write lineage as long as its reads expose the same effective
history. Forking is accepted only for an empty session or after a terminal turn
event. Mid-turn points remain inspectable but are not branch boundaries because
continuing there could silently repeat an external tool side effect.

Crash recovery is a separate operation. `Session::resume_pending` continues
only the newest incomplete turn at the current branch head. It never rewinds a
completed branch. Model and tool operations are at-least-once across the crash
boundary: a host that requires stronger side-effect guarantees supplies
idempotency keys or deduplication at its activity boundary.

## Store contract

`EventStore` separates three access patterns:

- `head` reads the current version without loading the stream body;
- `read_page(EventRange)` returns a bounded page, continuation point, and the
  head observed by that read;
- `append_batch` atomically advances a branch against an expected version.

`read` and `read_after` remain conveniences for callers that intentionally want
an unbounded projection. Engine write paths use `head`, not a full read.

## Snapshots

`SnapshotStore` is separate from `EventStore`. A `ProjectionSnapshot` is named,
schema-versioned, and anchored to a `SessionPoint`. It is a disposable replay
accelerator:

```text
reduce(snapshot, events_after_snapshot) == reduce(all_events)
```

Events remain authoritative. A missing, stale, or unknown snapshot must only
cost replay time; it cannot change the reconstructed result.

## Context

`ContextAssembler` receives a `ContextRequest` containing the exact session
point, turn and iteration, effective model, optional token limit, replayed
messages, and paged access to the event store. It returns a `ContextAssembly`
with provider messages, optional token accounting, provenance, and
observational context events. The engine persists those events before invoking
the model.

Context compaction and event-history compaction are distinct. Summarizing what
the model sees does not shorten replay or storage. Bounded pages and snapshots
remove accidental full reads and permit efficient projections, but a logical
session that truly runs forever still needs host-level archival or a future
generation-rollover operation. Such rollover must retain an auditable link to
the sealed parent history rather than making a summary a second hidden source
of truth.
