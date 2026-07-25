# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning: agentyk is pre-1.0 and stays strictly on `0.1.x` until a public
release — every release bumps the patch component (`0.1.z`). See
[`specs/release.md`](specs/release.md).

## [Unreleased]

### Fixed

- **A log written by a newer agentyk is readable by an older one.**
  `EventData` is internally tagged, so an unrecognized `kind` aborted
  deserialization of the line — and `JsonlEventLog::read` turned that into a
  failed read of the *whole session*, making it unresumable. `Event` now
  deserializes tolerantly: an unknown kind degrades to `EventData::Custom`
  carrying the original payload. Replay stays sufficient to resume a session
  across versions, which is the point of the persistence seam.

### Changed — composition lives in one value

- **`AgentConfig` (new, in `agentyk-core`) is an agent's whole composition** —
  prompt, model, capabilities, drivers, listeners, hooks, budget checker,
  context assembler, extensions. `AgentBuilder` fills one in and `Agent` holds
  it behind an `Arc`; `Agent::config()` exposes it.
- **`TurnHost` went from 15 public fields to 6**: `session_id`, `config`,
  `model` (the per-turn effective one), `log`, `messages`, `cancellation`,
  built with `TurnHost::new(..).model(..).cancellation(..)`.

  Adding a composition knob used to mean seven mechanical edits across four
  files — builder field, builder setter, `AgentInner` field, `build()` copy,
  `Agent` accessor, `TurnHost` field, session wiring — and the `TurnHost`
  field made it a breaking change for every third-party `TurnExecutor`. It is
  now a field on `AgentConfig` plus a builder setter, and reaches every
  executor for free.
- Executors read composition through `host.config.*`. Per-field `Agent`
  accessors are gone except `name()`, `model()`, `capabilities()` and
  `driver_for_model()`; use `agent.config()` for the rest.

### Changed — packaging and API-stability guardrails

- **Public data types are `#[non_exhaustive]`**, so core can grow fields and
  variants without a breaking change: `EventData`, `Error`, `LlmErrorKind`,
  `Event`, `EventRequest`, `ModelSpec`, `ReasoningConfig`, `ChatRequest`,
  `ChatResponse`, `Usage`, `ToolDefinition`, `ToolOutput`, `ToolContext`,
  `ToolPolicy`, `TurnState`, `PendingCall`, `TurnResult`,
  `CommandDescriptor`, `CommandContext`, `SystemPromptContext`, `FileEntry`,
  `McpServer`, `RunOptions`, `SimTurn`, `SimToolCall`. Each gained a
  constructor (and setters where it has optional parts) — e.g.
  `ChatRequest::new(model, messages).system_prompt(..).tools(..)`,
  `ToolContext::new(session, turn).with_extensions(..)`,
  `Usage::new(..)`, `FileEntry::file(..)` / `FileEntry::dir(..)`,
  `RunOptions::new().cancellation(..).controls(..)`.

  The **contract** types stay deliberately exhaustive — `TurnAction`,
  `TurnOutcome`, `Role`, `ContentPart`, `PreToolUseDecision`. A host that
  doesn't handle a new turn action, or a driver that doesn't translate a new
  content part, is wrong, and the compile error is the point. The rule is
  documented on `agentyk_core`'s crate docs.
- `SimTurn::tool_calls([..])` scripts a multi-tool batch in one turn, which
  previously required a struct literal.

- **`agentyk` defaults to no features.** `tokio` is now an optional dependency
  pulled in only by the features that use it (`mcp`, `fs`), each enabling just
  the tokio features it needs. Previously `tokio` — including `process` and
  `fs` support — was unconditional, so the documented "lean build" was not
  actually lean. Default build: 28 crates; `full`: 100. Use
  `features = ["full"]` for the previous batteries-included surface.
- **MSRV is declared and verified.** `rust-version = "1.88"` on every member,
  with a CI job that builds on exactly that toolchain, so the promise cannot
  silently drift.
- **`unsafe_code = "forbid"`** workspace-wide.
- **CI feature matrix** — each feature is built alone, not only in the
  all-features build, so `#[cfg]` gating stays honest as features grow.
- **`cargo-semver-checks` gates publishing** against the last crates.io
  release (in `publish.yml`, where a baseline exists; skipped for a crate's
  first publish).
- Feature-gated items carry `doc(cfg(...))` badges on docs.rs.

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

- **`codenko` example** (`examples/codenko`, `publish = false`): a terminal
  coding agent built on the public contract — `FileSystemCapability` plus a
  `run_command` shell tool, a `PreToolUseHook` that turns mutating calls into an
  in-TUI approval prompt, per-turn `CancellationToken`, and a transcript folded
  entirely from the event stream (so the display is tested with `SimDriver`, no
  terminal and no network). UI is [`tuika`](https://crates.io/crates/tuika).
  Runs on either driver — `--reasoning-effort` covers OpenAI's `gpt-5.6` family,
  which refuses function tools on chat completions unless the level is `none`.
  `demo.tape` records the README GIF with `vhs`, at 2x density for a display
  width of 880.

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
- **`agentyk-everruns-poc` richer narration**: the `EverrunsExecutor` now emits
  its tool risk hints and pre-run redactions as `EventData::Custom` events
  (`tool.hint` / `tool.rewritten`), and `NarrationListener` renders them
  (`🔎`/`⚠`/`✎`) plus provider extended thinking (`💭`, from `Message.thinking`)
  — the everruns transcript's richer signal, still a pure event observer with no
  core variant. The `transcript` example shows the upgraded output.

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
