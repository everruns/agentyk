# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning: agentyk is pre-1.0 and stays strictly on `0.1.x` until a public
release — every release bumps the patch component (`0.1.z`). See
[`specs/release.md`](specs/release.md).

## [Unreleased]

### Added — protocol extensibility (from an audit against everruns 0.17.16)

- **Reasoning round-trip**: `Message.thinking` and `Message.thinking_signature`
  carry provider "extended thinking" so it round-trips back to the model on
  later turns (typed, because the driver only ever sees a `Message`).
- **Streaming correlation**: a `MessageId` on `output.message.started` /
  `delta` / `completed` ties one streaming assistant message together (held on
  `TurnState::current_message_id`).
- **Generic metadata hatches** for everruns-flavored richness a satellite crate
  owns (tool risk/hint taxonomy, capability status/category, message phase):
  `ToolDefinition.metadata`, `Message.metadata`, and `Capability::metadata()` —
  the data analogue of `EventData::Custom`.

All fields are additive and serde-optional, so plain messages/tools/events
serialize as before and pre-0.1.1 logs still load. Behavior (mutating/approval
hooks, parallel dispatch) is deliberately left to a satellite `TurnExecutor` —
see [`specs/extensibility.md`](specs/extensibility.md).

- **`agentyk-everruns-poc` proof of concept** (`poc/agentyk-everruns-poc`,
  `publish = false`)
  — a proof that the extensibility boundary holds: a custom `TurnExecutor` with
  hint-based tool approval and a `ToolHints` taxonomy in the `metadata` hatch,
  built over `agentyk-core`'s public seams with no core change. Its library
  depends on core alone (no framework, no tokio).
- **Anthropic extended thinking** (feature `http`): `ReasoningConfig.budget_tokens`
  (+ `ModelSpec::thinking_budget` / `TurnControls::thinking_budget`); the
  Anthropic driver enables thinking per request, parses `thinking` blocks from
  responses into `Message.thinking`/`thinking_signature` (streaming included),
  and replays them with their signature on the next turn — completing the
  reasoning round-trip.
- **`agentyk-everruns-poc` extensions**: the satellite `EverrunsExecutor`
  now **dispatches a tool batch concurrently** (`pending_tool_actions` +
  `join_all`), closing agentyk's item-9 "concurrent dispatch" follow-up outside
  core; a `NarrationListener` showing the transcript surface is a pure
  `EventListener`; and a `PreToolGuard` chain (`GuardOutcome::{Allow, Rewrite,
  Deny}`) proving the remaining gap-4 shapes — **rewrite a call before it runs**
  (redaction), guard composition, and a **capability that contributes the guard
  gating its own tool**. Plus a runnable `transcript` example
  (`cargo run -p agentyk-everruns-poc --example transcript`) and a crate README.
- **`agentyk-everruns-poc` memory/compaction**: a `MemoryAssembler`
  implementing core's `ContextAssembler` — injects a persistent memory note into
  every turn and can cap replayed history (`keep_last`), showing everruns-style
  memory + compaction shape *what the turn sends* over the existing seam while
  the untrimmed history stays in the event log. No core change.

### Changed

- `TurnState::on_reason_started` now takes `&mut self` (allocates the reason
  step's `message_id`); the `output.message.*` event variants gained a
  `message_id` field. Internal — the `run`/executor API is unchanged.
- **Docs → specs.** The internal design docs (`plan`, `everruns-adoption`,
  `extensibility`) moved out of `docs/` into `specs/`, which is now an
  [Open Knowledge Format](https://okf.md) v0.1 bundle (per-file `type`
  frontmatter + `specs/index.md`). `docs/` is reserved for public product docs
  (none yet — `README.md` is the entry point). Repo-internal only.

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
