# Everruns adoption — gap analysis

What agentyk is still missing before `everruns-core`/`everruns-runtime` can be
rebuilt on top of it (Phase 2). Grounded in a survey of everruns' actual
public surface (`crates/core`, `crates/runtime`) against agentyk `0.1.0`.

Gaps are tiered by *where* they must land. The packaging rule applies
throughout: contract changes go to `agentyk-core`, machinery to `agentyk`,
and anything host-specific stays in everruns as a layer.

---

## Tier 1 — protocol gaps (core; do first, they break serialization)

These change types that are serialized into the event log, so they get more
expensive to change the longer we wait.

1. ✅ **Streaming — done.** `EventData` now has `OutputMessageStarted` /
   `OutputMessageDelta` / `OutputMessageCompleted` (`Replaced` and
   `ReasonThinking*` are not yet ported — no guardrail-replacement or
   extended-thinking capability exists to need them). `EventData::is_ephemeral()`
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

5. ✅ **Cancellation — done.** `cancellation::CancellationToken` (std-only,
   `Arc<AtomicBool>`-backed, no tokio dependency, so it lives in core) is a
   field on `TurnHost`, checked by `InProcessExecutor` once per action
   (between reason/tool steps) and once per streaming chunk (inside
   `RecordingDeltaSink::delta`, so cancellation lands mid-stream rather than
   waiting for a whole completion). `TurnState::on_cancel` is the pure
   transition to `TurnOutcome::Cancelled`, emitting `turn.cancelled`.
   `Session::run` is uncancellable by construction (throwaway token);
   `Session::run_cancellable(input, token)` takes a caller-held token to
   cancel from another task.

6. **Per-turn controls.** everruns `Controls` allows per-input model override
   and reasoning config; agentyk fixes the model at agent build time. Add a
   `run_with(input, TurnControls)` path where `TurnControls { model override,
   reasoning effort, … }` feeds `atoms::reason`. Also carry `ReasoningConfig`
   on `ModelSpec` so drivers can request thinking/effort.

## Tier 2 — machine & executor gaps (turn-semantics parity)

7. ✅ **Act hooks — partly done.** `PreToolUseHook` (`Allow` / `Deny{reason}`)
   and `PostToolExecHook` (transform the result) are in core
   (`hooks::{PreToolUseHook, PostToolExecHook, PreToolUseDecision}`),
   attached via `AgentBuilder::pre_tool_hook`/`.post_tool_hook`, and
   orchestrated by `InProcessExecutor` around `atoms::act` — **not inside
   the atom itself**, keeping hooks host/executor policy rather than a
   third atom. A denial short-circuits execution, emits a durable
   `tool.denied` event (`call_id`, `name`, `reason`) alongside the usual
   `tool.started`/`tool.completed` pair, and the reason becomes the
   (error) result the model sees. `atoms::act`'s signature is unchanged —
   hooks are an executor concern, so direct-atom callers (e.g. a durable
   host driving the machine manually) are unaffected unless they choose to
   run hooks themselves.
   **Not yet ported:** `require-approval` (a genuine pause-for-human-input
   decision needs a new `TurnPhase::PendingApproval` in the state machine —
   a bigger change deferred until a real approval capability needs it),
   `PostActHook` (turn-level, not per-tool — no use case yet), and
   `ClientSideToolHook` (client-executed tools — no client/server split
   exists in agentyk yet). Without `require-approval`, yolop's
   `ApprovalCapability` cannot fully port, but auto allow/deny guardrails
   (the more common case) can.

8. **Tool scheduling.** Everruns has a tool scheduler (parallel execution of
   a reason batch); agentyk drains `PendingAct` strictly serially. Generalize
   `PendingAct` to track per-call completion so an executor MAY run calls
   concurrently while the durable host keeps one-activity-per-call. Also
   missing: `ToolPolicy` / `DeferrablePolicy` / `ToolHints` (deferred tools
   are what ToolSearch is built on).

9. **Sealing and budget.** `TurnOutcome` lacks `Sealed(SealReason)`
   (no-progress crash loops, budget exhaustion) and there is no
   `BudgetChecker` seam or `Budget*` events. The machine gets a
   `seal(reason)` transition; the budget seam is a host-provided check the
   executor consults per iteration.

10. **Reason robustness.** One-shot `driver.complete()` today. Everruns has:
    LLM retry with backoff + error classification (uses gap 4), stream
    reconnect, `previous_response_id` chaining for stateful providers
    (OpenAI Responses), time-to-first-token and richer `TokenUsage`
    (cache/reasoning tokens). Retry policy belongs in the executor (durable
    hosts already retry activities — don't double-retry); response-id
    chaining needs a slot in `TurnState` (everruns keeps it in
    `RuntimeTurnState` too).

11. **Context assembly seam.** Agentyk sends the full replayed history every
    turn. Everruns assembles context (compaction, memory, trimming — with
    `ContextCompacting/Compacted` events). Add a `ContextAssembler` seam
    (default: passthrough of full history) between replay and
    `atoms::reason`; compaction then ports as an implementation, not a
    rewrite.

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

13. **Capability surface extras.** Missing from the trait, in rough priority:
    `commands()` (slash-command descriptors + an `execute_command` host API —
    yolop's `/goal`, `/setup` depend on this), `mcp_servers()` (capabilities
    contributing MCP servers, merged at session level), `dependencies()`,
    `status()` (enabled/degraded), `aliases()` (rename compatibility).
    Metadata like `icon`/`category`/localizations stays host-side.

14. **Richer `ToolContext`.** Everruns' `ToolContext` is a service bag
    (workspace id, file store, storage store, image store, credential store,
    utility LLM). Enumerating `Option<Arc<dyn …>>` fields in core would drag
    every host concern into the contract. Instead: add a typed extensions
    map (axum-style `Extensions`) to `ToolContext` — hosts inject services;
    tools downcast what they need; core stays lean. `workspace_id` (session
    vs shared workspace file keying) is worth first-classing when a
    filesystem capability lands.

15. **Session file system.** Everruns tools assume a `SessionFileSystem`
    (virtual FS, real-disk, write blocklists, mounts). Agentyk has none.
    This arrives as the first big framework capability
    (`FileSystemCapability` + `RealDisk`/`InMemory` stores) and is also the
    forcing function for gap 14.

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
6. ✅ Act hooks (pre/post tool) — unlocks guardrail ports; approval's
   require-approval path still needs a `TurnPhase::PendingApproval`
7. Per-turn `TurnControls`
8. Sealing (`Sealed(SealReason)`) + budget seam
9. Parallel-capable `PendingAct` + tool policy types
10. `ContextAssembler` seam
11. Capability `commands()` + `mcp_servers()`; `ToolContext` extensions
12. `FileSystemCapability` (first big bundled capability)

Items 1–5 are protocol-affecting and should land before anyone persists a
long-lived event log; 6–12 can land incrementally alongside early adoption
spikes (e.g. a `DurableExecutor` prototype in everruns after item 5).
