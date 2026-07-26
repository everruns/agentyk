---
type: Analysis
title: Everruns adoption — gap analysis
description: What agentyk still lacks before everruns-core/runtime can be rebuilt on top, tiered by where each gap must land.
tags: [everruns, adoption, gaps, protocol]
timestamp: 2026-07-24
---

# Everruns adoption — gap analysis

What agentyk is still missing before `everruns-core`/`everruns-runtime` can be
rebuilt on top of it (Phase 2). Grounded in a survey of everruns' actual
public surface (`crates/core`, `crates/runtime`) against agentyk `0.1.0`.

A companion analysis, [`yolop-adoption.md`](yolop-adoption.md), records what a
real coding agent hits when it is actually built on these seams; where the two
overlap they agree, and it orders by adopter pain rather than by layer.

Gaps are tiered by *where* they must land. The packaging rule applies
throughout: contract changes go to `agentyk-core`, canonical machinery to
`agentyk-engine`, bundled implementations to `agentyk`, and anything
host-specific stays in everruns as a layer.

---

## Tier 1 — protocol gaps (core; do first, they break serialization)

These change types that are serialized into the event log, so they get more
expensive to change the longer we wait.

1. ✅ **Streaming — done.** `EventData` now has `OutputMessageStarted` /
   `OutputMessageDelta` / `OutputMessageCompleted` (`Replaced` and
   `ReasonThinking*` are not yet ported — no guardrail-replacement or
   extended-thinking capability exists to need them). **Correlation (0.1.1):**
   all three carry a `message_id` (a typed `MessageId`) tying one streaming
   assistant message together — allocated in `on_reason_started`, held on
   `TurnState::current_message_id`, stamped onto the deltas and the completed
   event. `EventData::is_ephemeral()`
   classifies `OutputMessageDelta`; `Event.sequence` became `Option<u64>`
   (`None` for ephemeral, mirroring everruns exactly). `TurnHost::record`
   branches per-event: ephemeral events skip the `EventLog` entirely
   (`EventRequest::into_ephemeral_event`, no append, no history fold) and go
   straight to listeners; durable events persist as before. `ChatDriver`
   gained `complete_streaming(request, sink: &mut dyn DeltaSink)`, default
   `= complete()` + one synthetic full-text delta, so every driver streams
   without opting in. `atoms::reason_streaming` is the streaming sibling of
   `reason`; the machine itself stays sans-IO — `TurnState::on_reason_started`
   is the only new transition (pure, emits `output.message.started`), deltas
   never touch `TurnState` at all. `InProcessExecutor` wires a
   `RecordingDeltaSink` that forwards straight to `TurnHost::record`. Real
   incremental SSE streaming is implemented for both HTTP drivers — OpenAI
   Chat Completions (`delta.content` + indexed `delta.tool_calls[]` fragment
   accumulation) and Anthropic Messages (`content_block_delta` text/
   `input_json_delta` fragment accumulation) — parsing is unit-tested against
   synthetic SSE payloads (no live network, matching the existing driver
   testing bar); `SimDriver` streams via the default (one full-text delta).

2. ✅ **Multimodal messages — done.** `Message.content` is now
   `Vec<ContentPart>` (`Text`/`Image`, field-compatible with everruns'
   `TextContentPart`/`ImageContentPart`). Flat-string constructors
   (`Message::user`, `.assistant`, …) still take `impl Into<String>` and wrap
   a single `Text` part (or none, for empty content) — zero call-site churn.
   `Message::text()` derives the flat string for callers that only care about
   text (e.g. `TurnOutcome::Success { response }`).

3. **Event protocol extensibility.** Everruns has ~40 `EventData` variants
   (`Reason*`, `Act*`, `Budget*`, `ContextCompacting/Compacted`,
   `FileWritten`, `CapabilityUsage`, `LlmGeneration`, session lifecycle…).
   Agentyk's closed enum can't host them all at once. Two-part answer:
   - ✅ `EventData::Custom { event_type, payload }` — done. Escape hatch so
     hosts and capabilities can emit domain events without forking core;
     `EventData::event_type()` returns `&str` (was `&'static str`, to admit
     `Custom`'s owned type string) and durable by default (no custom
     *ephemeral* event exists yet — add one if a use case needs it).
   - adopt the everruns *frozen* protocol names incrementally as features
     land (compaction events arrive with the compaction seam, budget events
     with the budget seam) — `Custom` is the bridge until each graduates.

4. ✅ **Error taxonomy for retries — done.** `LlmErrorKind` (core, mirrors
   everruns' type) classifies `RateLimited`/`Overloaded`/`Timeout`/`Network`/
   `ServerError` (retryable) vs `Authentication`/`InvalidRequest`/`Unknown`
   (not — a config problem or unexpected shape won't fix itself). `Error`
   changed from `Driver(String)` to `Driver { kind, message }`, with
   `Error::is_retryable()` delegating to the kind (every other variant is
   non-retryable by construction). Both HTTP drivers classify by HTTP status
   (`classify_status`, including Anthropic's 529 overloaded code) and by
   transport failure kind (`network_error`: timeout vs. generic network,
   via `reqwest::Error::is_timeout()`). User-facing error *mapping* (i18n,
   display strings) is not ported — no UI layer exists yet to need it.

5. ✅ **Cancellation — done, and it now reaches inside a tool call.**
   `run_until_cancelled` races any future against the token and drops it when
   cancellation wins; the in-process host runs every tool call through it, and
   `ToolContext::cancellation` gives tools and middleware the token. See
   [`yolop-adoption.md`](yolop-adoption.md) gap 1.

   Originally: `cancellation::CancellationToken` (std-only,
   `Arc<AtomicBool>`-backed, no tokio dependency, so it lives in core) is a
   field on `TurnHost`, checked by `InProcessExecutor` once per action
   (between reason/tool steps) and once per streaming chunk (inside
   `RecordingDeltaSink::delta`, so cancellation lands mid-stream rather than
   waiting for a whole completion). `TurnState::on_cancel` is the pure
   transition to `TurnOutcome::Cancelled`, emitting `turn.cancelled`.
   `Session::run` is uncancellable by construction (throwaway token);
   `Session::run_cancellable(input, token)` takes a caller-held token to
   cancel from another task.

6. ✅ **Per-turn controls — done.** `controls::TurnControls { model,
   reasoning }` — `Session::run_controlled(input, controls)` overrides the
   model and/or reasoning effort for one turn without rebuilding the agent;
   `reasoning` layers onto whichever model is chosen (override or default),
   so bumping effort doesn't require also overriding the model. `ModelSpec`
   gained `reasoning: Option<ReasoningConfig>` (+ `.reasoning_effort(...)`
   builder method); the OpenAI driver forwards it as `reasoning_effort` in
   the request body. **Anthropic extended thinking (0.1.1) — done.**
   `ReasoningConfig` gained `budget_tokens: Option<u32>` (+
   `ModelSpec::thinking_budget(n)` / `TurnControls::thinking_budget(n)`),
   since Anthropic enables thinking with an integer budget, not an effort
   string. The Anthropic driver now: sends `thinking: { type: "enabled",
   budget_tokens }` (growing `max_tokens` past the budget as the API
   requires); parses `thinking` content blocks from responses into
   `Message.thinking` / `thinking_signature` (non-streaming and streaming —
   `thinking_delta`/`signature_delta` accumulate separately and are **not**
   surfaced as answer text); and replays them as a leading `thinking` block
   (with signature) on the next turn. Unit-tested against synthetic payloads,
   matching the driver testing bar (no live key).
   `Session::run_with_options(input, RunOptions { cancellation, controls })`
   is the general form; `run`/`run_cancellable`/`run_controlled` are thin
   wrappers over it.

## Tier 2 — machine & executor gaps (turn-semantics parity)

7. ✅ **Act hooks — done, as middleware.** Core exposes a single
   `middleware::TurnMiddleware` trait with defaulted methods
   (`before_tool` → `ToolCallDecision::{Proceed, Rewrite, Deny}`,
   `after_tool` → transform the result), attached via
   `AgentBuilder::middleware` and orchestrated by `TurnEngine` around tool
   operations — **not inside the tool itself**. A denial short-circuits
   execution and emits a durable `tool.denied`; a rewrite goes through
   `TurnState::on_tool_rewritten`, so it is durable, `tool.started` announces
   the call as it will actually run, and a resumed host executes the rewrite
   rather than the original. `atoms::act`'s signature is unchanged.

   This replaces the earlier `PreToolUseHook` / `PostToolExecHook` pair, which
   could only allow or deny and therefore did **not** cover everruns
   guardrails that **mutate/redact** a call. That gap previously forced a
   satellite to fork the whole act loop; it no longer does. Approval still
   needs no `TurnPhase::PendingApproval` — everruns itself has none (its
   `TurnPhase` is still PendingInput/Reason/Act/Completed) and gates via the
   hook plus a human-intent capability, which is exactly what
   `agentyk-everruns-poc`'s `ApprovalMiddleware` is.

   Two things stay outside core, correctly: **capability-contributed** guards
   (a capability contribution may bundle a tool with its middleware)
   and **dispatch strategy** (a host dispatcher concurrently fans out the
   engine's prepared batch).
   `PostActHook` (turn-level) and `ClientSideToolHook` (client/server split)
   remain unported — no use case yet, and each would be a defaulted method on
   `TurnMiddleware` rather than a new trait.

   **Known limit:** middleware governs the act phase. A rewrite cannot redact
   what the model already generated — `output.message.completed` still holds
   the original call. That needs a reason-phase interception point (everruns'
   `output.message.replaced`), still unported.

8. ✅ **Tool scheduling — engine prepares batches.**
   `TurnPhase::PendingAct` is now `{ calls: Vec<PendingCall> }`
   (`PendingCall { call, started, output }`) instead of a front-only queue
   — `on_tool_started`/`on_tool_completed` take a `call_id` and act on any
   call in the batch, so completion order no longer has to match dispatch
   order. `TurnState::pending_tool_actions()` returns the whole not-yet-started
   batch at once. `TurnEngine` applies middleware to the batch and returns
   `TurnOperation::InvokeTools`; the in-process host dispatches sequentially,
   while a concurrent or durable host may schedule the prepared calls without
   replacing turn semantics. Also done:
   `tool::{ToolPolicy, DeferrablePolicy}` — a tool can mark itself
   `Deferred` to stay executable but be left out of the definitions
   `atoms::assemble` sends the model; default is `Never` (today's
   behavior, unchanged for every existing tool). **`ToolHints` (0.1.1):**
   everruns' per-tool risk/UI taxonomy (`readonly`/`destructive`/`open_world`,
   etc.) is **not** a typed core field — it rides in the generic
   `ToolDefinition.metadata` hatch, and engine middleware reads it for
   approval/risk gating (see [`extensibility.md`](extensibility.md)). Still no
   ToolSearch-style capability that *surfaces* deferred tools on demand —
   `Deferred` today just means "hidden," not "hidden until requested."

9. ✅ **Sealing and budget — done.** `TurnOutcome::Sealed(SealReason)`
   (`NoProgress` | `BudgetExhausted`) + `TurnState::on_seal` (pure
   transition, emits durable `turn.sealed`). `budget::BudgetChecker` is a
   host-supplied seam (`AgentBuilder::budget_checker`, `None` by
   default — never seals): `InProcessExecutor` checks it once per action,
   next to the cancellation check, and seals via `Err(Error::Sealed(..))`
   on `BudgetDecision::Seal`. Sealing abandons whatever's pending, same as
   cancellation, but is distinct: cancellation is caller-driven
   (`CancellationToken`), sealing is host-policy-driven (a budget rule).
   `NoProgress` exists as a type but nothing sets it yet — it's meaningless
   without a durable host's crash-reclaim loop; agentyk's in-process
   executor can't crash-loop. No `Budget*` warning/paused/resumed events
   (everruns' `budget.warning/paused/exhausted/resumed`) — only the
   terminal `turn.sealed` outcome; a host wanting graduated warnings can
   emit them via `EventData::Custom` today.

10. **Reason robustness.** One-shot `driver.complete()` today. Everruns has:
    LLM retry with backoff + error classification (uses gap 4), stream
    reconnect, `previous_response_id` chaining for stateful providers
    (OpenAI Responses), time-to-first-token and richer `TokenUsage`
    (cache/reasoning tokens). Retry scheduling belongs to the host (durable
    hosts already retry activities — don't double-retry); response-id
    chaining needs a slot in `TurnState` (everruns keeps it in
    `RuntimeTurnState` too).

11. ✅ **Context assembly seam — done.** `context::ContextAssembler` sits
    between replay and `atoms::reason`: `AgentBuilder::context_assembler`
    (default `PassthroughContextAssembler` — sends history unchanged,
    today's exact behavior) transforms `host.messages` into what a turn
    actually sends, without touching the log or the state machine. Proven
    with a trimming assembler: the model sees only the trimmed view, while
    `session.messages()` still holds the full untrimmed history — trimming
    is a per-turn view, not a mutation of what's recorded. **Not ported:**
    an actual compaction *implementation* (summarizing old turns) or the
    `context.compacting`/`context.compacted` events — those are for a
    Phase-2 compaction capability to add as an `ContextAssembler` impl; the
    seam is what makes that possible without another core change.

## Tier 3 — capability model gaps (needed to port everruns capabilities)

12. **Per-attachment config.** Everruns capabilities take JSON config
    (`tools_with_config`, `system_prompt_contribution_with_config`,
    `config_schema` / `validate_config`) because registry-mediated wiring
    needs data-driven configuration. Agentyk's value-first answer: configure
    the object at construction (`WebFetch::builder().allowlist(…)`), which
    covers embedders. For Phase 2's declarative/DB capabilities, everruns
    layers `AgentCapabilityConfig { ref, config } → construct configured
    value` on top. Decision to hold: **do not** add `*_with_config` duality
    to the core trait; add `config_schema()` only, for hosts that render
    config UIs.

13. ✅ **Capability surface extras — `commands()` done, `mcp_servers()` held
    back.** `Capability::commands() -> Vec<CommandDescriptor>` and
    `execute_command(name, args, &CommandContext) -> Option<ToolOutput>`
    (default: none / `None`) are on the trait; `Session::commands()` lists
    every capability's descriptors and `Session::execute_command()` routes to
    the first capability that claims the name — entirely outside the turn
    loop (no model call, no event log entry), matching yolop's `/goal`,
    `/setup`. `mcp_servers()` is **not** implemented this pass: the config
    type (`McpServer`) lives in the framework crate behind the `mcp` feature,
    but `Capability` lives in `agentyk-core`, which cannot depend on the
    framework crate — porting it needs either a core-side `McpServer` config
    struct (pure data, no client) or a different seam (e.g. the framework
    crate composing capabilities' server lists post hoc). Left as a follow-up
    to resolve if/when an MCP-contributing capability is actually built.
    `dependencies()`, `status()` (enabled/degraded), `aliases()` (rename
    compatibility) remain unported — no adopter has needed them yet.
    Metadata like `icon`/`category`/localizations stays host-side.

14. ✅ **Richer `ToolContext` — typed extensions, not a service enum.**
    Everruns' `ToolContext` is a service bag (workspace id, file store,
    storage store, image store, credential store, utility LLM). Rather than
    enumerating `Option<Arc<dyn …>>` fields in core (which would drag every
    host concern into the contract), `ToolContext` gained
    `extensions: agentyk_core::extensions::Extensions` — a typed
    `TypeId`-keyed bag (axum's `Extensions` pattern: `insert::<T>`,
    `get::<T>() -> Option<Arc<T>>`, `contains::<T>()`). Hosts populate it via
    `AgentBuilder::extension(value)`, stored once on `Agent` and cloned into
    every `ToolContext`; tools downcast what they need. `workspace_id`
    (session vs shared workspace file keying) is still worth first-classing
    when a filesystem capability lands (gap 15), since every tool needs it,
    not just some.

15. ✅ **Session file system — `FileSystemCapability` (feature `fs`,
    default-on).** `filesystem::FileSystem` (`agentyk` crate) mirrors
    everruns' `SessionFileSystem` shape (`read_file`/`write_file`/
    `list_directory`/`delete_file`, plain `&str` paths, async) with one
    deliberate simplification: **no `session_id`/`workspace_id` parameter**.
    One store is one workspace — the same behavior everruns'
    `RealDiskFileStore` already has in practice (it accepts `session_id` but
    ignores it). Multi-workspace hosts compose this by attaching a different
    `FileSystemCapability` per agent/session rather than routing through a
    shared, keyed store; first-classing `workspace_id` (gap 14's note) is
    deferred until an adopter actually needs one store shared across
    sessions.
    - `RealDiskFileSystem` — rooted at a canonicalized directory; every path
      is resolved component-by-component with `..` rejected structurally
      (never touches the OS to check containment — there is no path that can
      escape the root regardless of what the model sends). No symlink
      rejection or mount layer (everruns' `reject_symlink_path`, `MountFs`)
      — out of scope for this pass.
    - `InMemoryFileSystem` — pure `HashMap`-backed VFS for tests/hosts that
      don't want real disk I/O; directories are inferred from `/`-separated
      key prefixes, not stored explicitly.
    - `WriteBlocklistFileSystem` — decorator rejecting writes/deletes under
      `.git`/`node_modules`/`target`/`dist`/`build` by default (configurable),
      composes over either store; mirrors everruns'
      `WriteBlocklistFileStore`. `ApprovalGatingFileStore` not ported (no
      approval-gate capability exists yet — see gap 6's require-approval
      note).
    - `FileSystemCapability` now exposes 7 tools: `read_file` (with
      `offset`/`limit` line windows), `write_file`, `edit_file` (exact-string
      replacement refused on an ambiguous match, rather than everruns'
      content-hash CAS — same guarantee, expressed so the model can retry with
      more context), `list_directory`, `grep_files` (regex, recursive,
      bounded), `stat_file`, and `delete_file`. All are written against the
      `FileSystem` trait, so they work over any store; `FileSystem::stat` is
      defaulted in terms of `list_directory`. Still not ported:
      content-type-aware read defaults and byte caps. Every definition carries
      risk hints in `ToolDefinition.metadata`. See
      [`yolop-adoption.md`](yolop-adoption.md) gap 5.

## Tier 4 — host-side by design (non-gaps)

Confirmed everruns surface that should **not** move into agentyk; Phase 2
implements these as layers over the seams:

- Harness → Agent → Session hierarchy (agent templates over `Agent` values)
- org/tenant scoping, principals, auth, provider catalog + key encryption
- declarative/DB capabilities, plugin compiler, capability registry-by-id
- durable engine internals (workflow store, task queue, reclaim, retries)
- NATS/SSE event delivery, reporting outbox/facts
- background tasks, scheduling, platform store, subagent spawning
- memory/knowledge stores, vector stores, session SQL DB
- A2A, OpenAPI generation, localization metadata

## Notable design differences to hold (not gaps)

- **No `MessageId`.** In agentyk the input message *is* an event; turn
  correlation uses `TurnId` + event ids. Everruns' `input_message_id`
  maps to the `input.message` event id.
- **History = fold over the log.** Everruns loads messages from stores;
  agentyk derives them (`replay`). The compaction seam (gap 11) is where the
  "don't replay everything forever" story lands — not a message store trait.
- **Capabilities are configured values**, not registry entries with config
  blobs (gap 12 decision).

## Recommended attack order

Phase 1.5 (pre-adoption hardening, in this repo):

1. ✅ Multimodal `ContentPart` message body (protocol, cheapest now) — done:
   `Message.content` is `Vec<ContentPart>` (`Text` / `Image`, matching
   everruns' field shapes); `Message::text()` derives the flat string; both
   HTTP drivers send a plain string on the wire for text-only messages and a
   content-part array when an image is present (OpenAI `image_url`,
   Anthropic `image` blocks). Tool calls/results stay dedicated `Message`
   fields rather than folding into content parts — see "notable design
   differences" below.
2. ✅ Streaming: delta events + ephemeral classification + streaming driver API
3. ✅ Cancellation + `TurnOutcome::Cancelled`
4. ✅ `EventData::Custom` escape hatch
5. ✅ Error retryability classification
6. ✅ Act hooks (pre/post tool) — builder-attached guardrails. Mutation and
   approval are middleware behavior orchestrated by the engine; a
   capability contribution may bundle its own middleware. This is **not** a
   core approval phase (see [`extensibility.md`](extensibility.md)).
7. ✅ Per-turn `TurnControls`
8. ✅ Sealing (`Sealed(SealReason)`) + budget seam
9. ✅ Parallel-capable `PendingAct` data model + tool policy types —
    concurrent dispatch in `InProcessExecutor` landed later, via
    `concurrency::join_all` (see [`yolop-adoption.md`](yolop-adoption.md) gap 6)
10. ✅ `ContextAssembler` seam
11. ✅ Capability `commands()` + `ToolContext` extensions —
    `mcp_servers()` held back (core/framework layering conflict, see gap 13)
12. ✅ `FileSystemCapability` (first big bundled capability) — see gap 15

All twelve Phase 1.5 items are now done (with the scoping notes above); see
[`plan.md`](plan.md#phase-1-status) for the full status list.

Phase 1.6 (protocol extensibility, 0.1.1) — from an audit against everruns
0.17.16 (see [`extensibility.md`](extensibility.md) for the strategy):

1. ✅ `message_id` correlating the `output.message.*` streaming events (typed).
2. ✅ `Message.thinking` + `thinking_signature` (typed reasoning round-trip).
3. ✅ Generic `metadata` hatches — `Message.metadata`,
   `ToolDefinition.metadata` (home for `ToolHints`), `Capability::metadata()`.
4. Hook mutation / approval / capability-contributed guardrails → engine
   middleware and capability contributions, not a custom whole-turn executor.

Items 1–5 are protocol-affecting and should land before anyone persists a
long-lived event log; 6–12 can land incrementally alongside early adoption
spikes. Durable adoption should prototype a host of the canonical step engine,
not a `DurableExecutor` that copies the loop.
