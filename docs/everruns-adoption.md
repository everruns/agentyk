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

1. **Streaming.** Everruns' protocol has `OutputMessageStarted / Delta /
   Replaced / Completed` plus `ReasonThinking*` deltas, and classifies events
   as **ephemeral** (deltas — delivered, never persisted, no sequence) vs
   **durable**. Agentyk has a single `OutputMessage` and every event is
   durable. Needed:
   - delta variants in `EventData` + an `is_ephemeral()` classification;
   - `EventLog` semantics for ephemeral events (deliver to listeners, skip
     append) — likely a `record` path change, not a trait change;
   - a streaming API on `ChatDriver` (e.g. `complete_streaming` returning a
     chunk stream, default-implemented via `complete`);
   - the machine stays sans-IO: deltas are *listener traffic*, only the
     completed message is a transition input.

2. **Multimodal messages.** everruns `Message` content is structured
   (`ContentPart`: text, image, …); agentyk `Message.content` is a flat
   `String`. Because `Message` is embedded in `input.message` /
   `output.message` events, this is a protocol change — do it before any log
   format matters. Flat-string constructors stay as conveniences.

3. **Event protocol extensibility.** Everruns has ~40 `EventData` variants
   (`Reason*`, `Act*`, `Budget*`, `ContextCompacting/Compacted`,
   `FileWritten`, `CapabilityUsage`, `LlmGeneration`, session lifecycle…).
   Agentyk's closed 7-variant enum cannot host them. Two-part answer:
   - adopt the everruns *frozen* protocol names incrementally as features
     land (compaction events arrive with the compaction seam, budget events
     with the budget seam);
   - add an `EventData::Custom { event_type, payload }` escape hatch now, so
     hosts and capabilities can emit domain events without forking core.

4. **Error taxonomy for retries.** Durable execution retries activities, so
   it must distinguish retryable LLM failures (rate limit, overload,
   transient network) from terminal ones (auth, invalid request). Everruns
   has `LlmErrorKind` + user-facing error mapping; agentyk's
   `Error::Driver(String)` erases this. Add a retryability classification to
   driver errors in core; drivers populate it.

5. **Cancellation.** No way to stop a running turn. Every interactive host
   (yolop's Esc, everruns' stop button) needs it, and the durable engine
   needs cooperative cancellation between activities. Add a cancellation
   token to `TurnHost` (checked by the executor between actions) and a
   `TurnOutcome::Cancelled` + `turn.cancelled` event.

6. **Per-turn controls.** everruns `Controls` allows per-input model override
   and reasoning config; agentyk fixes the model at agent build time. Add a
   `run_with(input, TurnControls)` path where `TurnControls { model override,
   reasoning effort, … }` feeds `atoms::reason`. Also carry `ReasoningConfig`
   on `ModelSpec` so drivers can request thinking/effort.

## Tier 2 — machine & executor gaps (turn-semantics parity)

7. **Act hooks.** Everruns' act pipeline is extensible: `PreToolUseHook`
   (allow / deny / require-approval — this is what approval gating and
   guardrails are built on), `PostToolExecHook`, `PostActHook`,
   `ClientSideToolHook`. Agentyk executes tools with no interception point.
   Add hook traits in core and honor them in `atoms::act` / the executor.
   Without this, yolop's ApprovalCapability and everruns' guardrails cannot
   port.

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

1. Multimodal `ContentPart` message body (protocol, cheapest now)
2. Streaming: delta events + ephemeral classification + streaming driver API
3. Cancellation + `TurnOutcome::Cancelled`
4. `EventData::Custom` escape hatch
5. Error retryability classification
6. Act hooks (pre/post tool) — unlocks approval + guardrails ports
7. Per-turn `TurnControls`
8. Sealing (`Sealed(SealReason)`) + budget seam
9. Parallel-capable `PendingAct` + tool policy types
10. `ContextAssembler` seam
11. Capability `commands()` + `mcp_servers()`; `ToolContext` extensions
12. `FileSystemCapability` (first big bundled capability)

Items 1–5 are protocol-affecting and should land before anyone persists a
long-lived event log; 6–12 can land incrementally alongside early adoption
spikes (e.g. a `DurableExecutor` prototype in everruns after item 5).
