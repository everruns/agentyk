# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning: agentyk is pre-1.0 and stays strictly on `0.1.x` until a public
release — every release bumps the patch component (`0.1.z`). See
[`specs/release.md`](specs/release.md).

## [Unreleased]

## [0.1.0] - 2026-07-11

First published release. Two crates, versioned in lockstep:

- **`agentyk-core`** — the contract crate: traits, the event protocol, and the
  sans-IO turn state machine. Lean by construction (no tokio, no HTTP, no
  process spawning); this is what a durable/server host implements against.
- **`agentyk`** — the framework crate: builders, the in-process executor, the
  JSONL event log, bundled drivers, MCP, and the filesystem capability.
  Re-exports all of core, so applications depend only on `agentyk`.

### Highlights

- **Compose agents from values.** `Agent`/`AgentBuilder` — a system prompt, a
  `ModelSpec`, capabilities, and tools, with no entity creation, registration,
  or ids to thread. The everruns `input → reason → act` turn contract, run
  by a pluggable `TurnExecutor` (default `InProcessExecutor`) over a serializable
  `TurnState` machine.
- **Event log as the persistence seam.** `InMemoryEventLog` and `JsonlEventLog`;
  a session is resumable purely by replaying its log. Streaming deltas are
  ephemeral (never persisted).
- **Multi-provider drivers.** Scripted `SimDriver` for offline/deterministic
  tests; OpenAI and Anthropic HTTP drivers with real incremental SSE streaming
  behind the `http` feature.
- **Extensible turn.** Pre/post tool hooks, per-turn `TurnControls`, cancellation,
  deliberate sealing + a budget seam, a parallel-capable act data model, tool
  policy, and a pluggable context-assembly seam.
- **Capabilities.** Async tool discovery, host-invoked slash `commands()`, a
  typed `ToolContext` extensions bag, MCP over stdio, and a bundled
  `FileSystemCapability` (real-disk / in-memory stores, write blocklist).

**Full Changelog**: https://github.com/everruns/agentyk/commits/v0.1.0
