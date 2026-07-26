# Changelog

All notable changes to this project are documented here. The format is based
on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

Versioning: agentyk is pre-1.0 and stays strictly on `0.1.x` until a public
release — every release bumps the patch component (`0.1.z`). See
[`knowledge/release.md`](knowledge/release.md).

## [Unreleased]

### Added — what a real coding agent needed

Driven by building one: yolop's execution story was ported onto agentyk's
seams, and these are the gaps that port hit. See
[`knowledge/yolop-adoption.md`](knowledge/yolop-adoption.md).

- **Cancellation reaches inside a tool call.**
  `CancellationToken::run_until_cancelled` races any future against the signal
  and drops it when cancellation wins; `InProcessExecutor` runs every tool
  call through it, so a cancelled turn abandons a long build instead of
  waiting it out (a `kill_on_drop` child dies with the dropped future). The
  token is std-only still — no tokio, no futures crate — and now wakes parked
  futures rather than only being polled. `ToolContext::cancellation` exposes it
  to tools and, through `ToolInvocation::context`, to middleware: an approval
  prompt can stop waiting when the turn is cancelled.
- **Tools can report progress while they run.**
  `ToolContext::report_progress(ToolProgress)` emits an **ephemeral**
  `tool.progress` event to listeners — never persisted, never folded into
  history, same contract as a streaming delta. `ToolProgressSink` is the host
  seam; a tool with no host listening reports into a no-op.
- **Tool results can carry structure.** `ToolOutput::metadata` is a host-facing
  payload (exit codes, replacement counts, line windows) that rides on
  `tool.completed` while the model still receives just `content`.
- **`ModelSpec::metadata`** — a hatch for provider-flavored configuration
  (OAuth refresh tokens, account ids, organization ids, gateway headers) that
  `api_key` + `base_url` cannot express. Redacted in `Debug` alongside the key.
- **Anthropic prompt caching, on by default.** Up to four `cache_control`
  breakpoints per request — tools, system prompt, and the last two messages,
  the pair being what makes caching incremental across a growing transcript.
  Disable with `AnthropicDriver::prompt_caching(false)`. Cache-creation and
  cache-read tokens are now counted in `Usage::input_tokens`.
- **`FileSystemCapability` grew the missing coding tools**: `edit_file`
  (exact-string replacement, refused when the match is ambiguous),
  `grep_files` (regex, recursive, bounded), `stat_file`, and `offset`/`limit`
  line windows on `read_file`. All are written against the `FileSystem` trait,
  so they work over real disk, the in-memory store, or an adopter's own.
  `FileSystem::stat` is defaulted in terms of `list_directory`, so existing
  implementations gain it without changing.
- Every bundled filesystem tool declares risk hints in
  `ToolDefinition.metadata` under `"hints"`, so an approval middleware gates on
  what a tool says about itself instead of on a hard-coded list of names.

### Added — navigable session timelines

- Added immutable `SessionPoint`s, read-only historical `SessionView`s, and
  completed-turn `Session::fork` with durable `session.forked` lineage.
- Added head-only `Session::resume_pending` for explicit recovery of an
  incomplete turn, with documented at-least-once external-action semantics.
- `EventStore` now exposes efficient heads and bounded `EventRange` pages with
  continuation points. Engine writes no longer read the entire session merely
  to discover its expected version.
- Added the separate `SnapshotStore` contract and
  `InMemorySnapshotStore` for named, schema-versioned, disposable replay
  projections.
- Expanded context assembly into `ContextRequest`/`ContextAssembly`: policies
  receive the durable point, turn, iteration, model, optional token ceiling,
  replayed messages, and paged event access, and can return accounting,
  provenance, and durable observational events.

### Changed

- Aligned the knowledge bundle and repository guidance with the authoritative
  Google Cloud Open Knowledge Format v0.2 specification, including explicit
  bundle versioning, concept descriptions, and local conformance validation.
- Migrated the repository’s durable design specs into `knowledge/`, an OKF
  v0.2 bundle explicitly maintained as persistent repository memory. Agent guidance,
  shipping checks, and the definition of done now require integrating durable
  decisions and stale-claim cleanup with each change.

### Documentation

- Adopted the Everruns diagram system, with narrowly permitted semantic
  colors, and replaced the public architecture page's text sketches with
  co-located Mermaid sources and reviewed SVG renderings.
- Fixed the execution-cycle diagram so sequence lifelines remain visible and
  every message connects to its sender and receiver.

### Changed — one canonical engine, multiple execution hosts

- Added `agentyk-engine`, the shared home of `Agent`, `Session`, the
  prepare/apply step engine, and the in-process host. The top-level `agentyk`
  crate remains the application facade and keeps drivers, MCP, filesystem, and
  event-store implementations as feature-gated modules.
- Removed custom whole-turn executors from agent composition. `TurnEngine`
  applies middleware, budgets, cancellation, and transitions once, then hands
  provider-neutral model operations or prepared tool batches to a host.
- Durable hosts can discard `TurnState` at every boundary and reconstruct it
  from events. Turn-start, model-usage, and rewritten-call data are now
  persisted for that reduction.
- `EventStore` supports atomic batches, expected stream versions, and
  incremental reads. Live history advances only after persistence succeeds.
- Prepared operations remain runtime-only because model requests can contain
  credentials; durable hosts persist emitted events and resolve protected
  resources at execution time. `ModelSpec` debug output now redacts API keys.

### Fixed

- **A log written by a newer agentyk remains inspectable by an older one.**
  `EventData` is internally tagged, so an unrecognized `kind` aborted
  deserialization of the line — and `JsonlEventLog::read` turned that into a
  failed read of the *whole session*, making it unresumable. `Event` now
  deserializes tolerantly: an unknown kind degrades to `EventData::Custom`
  carrying the original payload. Unknown observational events remain readable;
  unknown state-bearing events now stop turn-state replay rather than risking
  an incorrect resume.

### Added — the HTTP drivers are tested over a real socket

- `crates/agentyk/tests/http_drivers.rs` serves canned provider responses from
  a local `TcpListener` and drives the real `ChatDriver` through
  `ModelSpec::base_url`, so the request goes out over a socket and comes back
  through the shared HTTP layer. Covers both providers, both the streaming and
  non-streaming paths, endpoint and auth-header construction, HTTP-error
  classification, and the shape-change error end to end. No new dependency —
  tokio is already a dev-dependency.
- What it still does not prove is that the canned bodies match what the
  providers send *today*; only a live call does that.

### Added — every public item is documented, and stays that way

- **270 undocumented public items now carry documentation** — enum variants,
  struct fields, trait methods, constructors — across both published crates
  and the proof-of-concept satellite. Field docs say what a field is *for*,
  not what it is named: why `ToolOutput::is_error` is a result rather than a
  turn failure, why `Message.thinking` must round-trip, what an `EventListener`
  can and cannot do.
- **`missing_docs = "deny"`** at the workspace level, so an undocumented
  public item fails `cargo check` locally at the same moment it would fail CI
  — rather than accumulating until someone runs a docs pass.

### Changed — provider wire types are typed, so a shape change is diagnosable

- **Both drivers deserialize provider payloads into typed structs** instead of
  indexing a `serde_json::Value` with `.unwrap_or_default()`. A renamed or
  missing field used to produce an empty assistant message with nothing to
  debug; it now produces
  `anthropic response did not match the expected shape: missing field 'content' at line 1 column 42`.
  The error is classified non-retryable, because replaying the same request
  cannot fix a shape change.
- Typed **both** paths, not just the non-streaming one — `complete_streaming`
  is what the default executor actually calls, so that is where a silent empty
  message was most likely.
- Tolerant where tolerance is right, strict where it is not: unknown *event
  types* and unknown *content-block types* deserialize to an ignored variant
  (providers add them routinely), while a known block or event whose fields
  changed fails. An empty OpenAI `choices` array is now an error rather than a
  blank turn.
- Error messages deliberately do not echo the response body — it holds the
  conversation. Serde's field name and position are enough to diagnose.
- `SseDecoder` yields raw `data:` payloads; decoding belongs to the
  accumulator, which is the only thing that knows the expected shape.
  OpenAI's `[DONE]` is recognized by `StreamAccumulator::is_terminator`
  instead of being silently swallowed as "not JSON".
- `serde` became an optional dependency of `agentyk`, enabled by `http`.

### Changed — module layout and an explicit public surface

- **`agentyk-core`'s physical files now match its public domain modules.**
  There are no private `protocol`/`agent`/`runtime` buckets hiding the actual
  API map.
- **`agentyk` lists its re-exports instead of `pub use agentyk_core::*`.**
  A glob means every future core item silently widens this crate's surface,
  including names that collide with one it already owns. `scripts/check_reexports.py`
  (wired into CI) fails if core exports something `agentyk` does not, so the
  explicit list cannot develop the mirror-image problem of a silent omission.
- **New `agentyk::prelude`** for the names most applications want at once.

### Changed — history is a projection, not a second source

- **`replay::History` (new) replaces the raw `Vec<Message>`** on `TurnHost`
  and `Session`. The log is the truth; a running turn still needs the history
  in memory rather than a log read per reason step, so a projection exists —
  but the only way to grow one is `History::apply(&EventData)`. A message that
  never became an event cannot enter history, so the projection and a fresh
  replay agree by construction rather than by an executor remembering to keep
  them in sync.
- `History::from_events` is the fold; `messages_from_events` stays as
  shorthand.
- Pinned by tests at both levels: in core, applying events one by one equals
  replaying the same log; end to end, `session.messages()` equals
  `messages_from_events(session.events())` after a multi-turn run with tool
  calls.

### Changed — HTTP drivers are wire mapping only

- **New crate-internal `drivers::http` layer** carrying what every HTTP
  provider shares: sending, HTTP-status and transport-error classification,
  SSE framing (`SseDecoder`), and the streaming loop. A provider now
  implements `HttpProvider` + `StreamAccumulator` and its `ChatDriver` impl is
  two delegations.
- The two drivers previously carried private copies of `classify_status`,
  `network_error`, `drain_lines` and `parse_sse_data_line`, plus their own
  send/status/decode dance and streaming loop — the parts most likely to
  drift apart. Per-provider production code: anthropic 469 → 408 lines,
  openai 413 → 339.
- **The streaming loop is now actually tested.** Both drivers' streaming tests
  used to re-implement the chunk loop in the test body, so the loop that ran
  in production had no coverage. Tests now drive the real loop via
  `drive_stream`, with bodies deliberately split mid-line to exercise
  reassembly.
- `AnthropicDriver::with_client` / `OpenAiDriver::with_client` take a
  `reqwest::Client`, so timeouts, proxies and pooling are configurable instead
  of hardcoded to `Client::new()`.
- OpenAI's `[DONE]` sentinel needs no special case: it simply is not JSON, and
  the shared decoder skips non-JSON payloads.

### Changed — one middleware seam instead of a trait per interception point

- **`TurnMiddleware` (new, in `agentyk-core`) replaces `PreToolUseHook` and
  `PostToolExecHook`.** One trait with defaulted methods — `before_tool`
  returning `ToolCallDecision::{Proceed, Rewrite, Deny}`, and `after_tool`
  transforming the result — attached with `AgentBuilder::middleware`.
  A new interception point becomes a defaulted method rather than a new trait,
  a new config field, a new builder setter, and new code in every executor.
- **Middleware can rewrite a call**, which the old hooks could not. Argument
  redaction and path rewriting previously forced a satellite to fork the whole
  executor loop just to get at the act phase.
- A rewrite is a **state transition** (`TurnState::on_tool_rewritten`), not
  just an event: `tool.started` announces the call as it will actually run,
  and a durable host resuming mid-turn executes the rewrite rather than the
  model's original call. New durable `tool.rewritten` event.
- `before_tool_chain` / `after_tool_chain` in core define the chain semantics
  once — a rewrite feeds the next middleware, the first deny short-circuits —
  so built-in, satellite and durable executors cannot drift on them.
- **Known limit, documented on `ToolCallDecision::Rewrite`:** middleware
  governs the act phase. Redacting a tool argument keeps the value out of the
  tool and out of every act-phase event, but `output.message.completed` still
  records the call the model generated. Redacting that needs a reason-phase
  interception point (everruns' `output.message.replaced`), which agentyk
  does not have.
- The `agentyk-everruns-poc` satellite lost its private `PreToolGuard` /
  `GuardOutcome` traits entirely; its `EverrunsExecutor` is now a unit struct
  whose only difference from the built-in executor is concurrent dispatch —
  which is what an executor should be for.

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
see [`knowledge/extensibility.md`](knowledge/extensibility.md).

- **`codenko` example** (`examples/codenko`, `publish = false`): a terminal
  coding agent built on the public contract — `FileSystemCapability` plus a
  `run_command` shell tool, a `PreToolUseHook` that turns mutating calls into an
  in-TUI approval prompt, per-turn `CancellationToken`, and a transcript folded
  entirely from the event stream (so the display is tested with `SimDriver`, no
  terminal and no network). UI is [`tuika`](https://crates.io/crates/tuika).
  Runs on either driver — `--reasoning-effort` covers OpenAI's `gpt-5.6` family,
  which refuses function tools on chat completions unless the level is `none`.
  Flags are parsed with `clap` (derive), so `--flag=value`, `--version`, and
  typo suggestions come for free; resolving them against the environment stays
  hand-written and unit-tested. `demo.tape` records the README GIF with `vhs`,
  at 2x density for a display width of 880.

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
  [Open Knowledge Format v0.2](https://github.com/GoogleCloudPlatform/knowledge-catalog/blob/main/okf/SPEC.md) bundle (per-file `type`
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
