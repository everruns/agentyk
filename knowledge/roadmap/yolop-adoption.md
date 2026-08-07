---
type: Analysis
title: Yolop adoption — what a real coding agent hits
description: Tracks reusable Yolop capabilities and the order in which Agentyk should adopt them.
tags: [adoption, yolop, gaps, drivers, tools, extensibility]
---

# Yolop adoption — what a real coding agent hits

[`everruns-adoption.md`](everruns-adoption.md) is a gap analysis derived from
reading everruns' public surface. This one is derived from **building**: yolop
grew a feature-gated second execution backend (`--engine agentyk`, in
`src/agentyk_backend/`) that composes an agent from agentyk's seams — model,
capabilities, tools, middleware, listeners, event log — and drives it for real
turns with a sandboxed shell.

The two analyses agree where they overlap; the value of this one is that
everything below was hit, not predicted, and the ordering is by how hard it
blocks a shipping coding agent rather than by where the fix lands.

The backend's own record of the port (what it covers, what it skipped) is
[`knowledge/specs/agentyk-backend.md`][yolop-backend]
in the yolop repository.

## What held up

Recorded first, because it is the load-bearing result: the composition model
worked. No seam had to be forked, and nothing needed a patch to core.

- **Capabilities by object.** An `AGENTS.md`/`CLAUDE.md` instructions
  capability and a workspace-tools capability were ~40 lines each, with no
  registry, no ids to thread, and no config plumbing.
- **Middleware for approval.** `before_tool` blocking on an answer *is* the
  approval flow; no `PendingApproval` phase was missed. `ToolInvocation`
  carrying `definition` matters more than it looks — the risk hints in
  `ToolDefinition.metadata` are readable at exactly the point the decision is
  made, so approval gates on what a tool declared rather than on its name.
- **The event stream as the only UI subscription.** The whole terminal
  renderer is one `EventListener`. Streaming deltas, tool starts, denials, and
  errors were all derivable from events; nothing had to reach into the session.
- **Filesystem defaults.** `RealDiskFileSystem`'s structural `..` rejection
  plus `WriteBlocklistFileSystem` gave a safe-by-default workspace for free.

## Blocking gaps

Ordered by how much they hurt. 1–4 are the ones that would stop yolop from
shipping on agentyk. **All four, plus gap 5, are now closed** — the
resolution is recorded under each.

1. ✅ **A tool call cannot be cancelled — fixed** (core + engine).

   *Was:* `InProcessExecutor` checked the
   cancellation token between actions and between streaming chunks, but awaits
   `atoms::act` unraced, and `ToolContext` carries no token. Ctrl-c during a
   two-minute `cargo test` is honored only when the command finishes. The fix
   is two-part and both parts are needed: race the tool future against the
   token in the host, **and** put the token on `ToolContext` so a tool can
   cooperate (kill its child rather than be abandoned). The same applies to a
   middleware awaiting an approval answer — a turn cancelled while a prompt is
   open cannot currently unwind.

   **Resolution.** `CancellationToken` now wakes parked futures (std-only, no
   tokio: a waker slot table on the shared state) and offers
   `run_until_cancelled(future)`, which drops the future when cancellation
   wins. `InProcessExecutor` runs every tool call through it, so an abandoned
   call is really abandoned — a `kill_on_drop` child dies with the future, and
   no `tool.completed` is recorded. `ToolContext::cancellation` carries the
   token to tools and, via `ToolInvocation::context`, to middleware, which is
   how an approval prompt stops waiting.
2. ✅ **Tools cannot report progress or return anything but a string —
   mostly fixed** (core + engine).

   *Was:* `ToolOutput` was `{ content: String, is_error: bool }` and
   `ToolContext` had no event sink. So: no streaming command output, no
   narration during a call, no background/long-running tools, no image or
   structured results. everruns' `BackgroundEventSink` and narration phases
   had no counterpart. A sink on `ToolContext` emitting `EventData::Custom`
   would unblock most of it without a protocol change; rich results need a
   content-part-shaped `ToolOutput`, which is a protocol change and therefore
   cheaper now than later.

   **Resolution.** `ToolContext::report_progress(ToolProgress)` emits an
   ephemeral `tool.progress` event through a host-supplied `ToolProgressSink`
   — never persisted, never folded into history, the same contract as a
   streaming delta. `ToolOutput::metadata` carries structured results to the
   host on `tool.completed` while the model still sees only `content`.
   Multimodal results followed: `ToolOutput::parts` carries content parts the
   model sees, recorded on `tool.completed` so a replay sends the same result.
   The asymmetry the deferral worried about is real and handled per driver —
   Anthropic nests them in the `tool_result` block, while the Chat Completions
   protocol has no slot for them, so the OpenAI driver relays them as a
   following user message rather than dropping them. `parts` is documented as
   best-effort for exactly that reason; `content` must stand on its own.
3. ✅ **No prompt caching — fixed** (drivers).

   *Was:* the Anthropic driver emitted no
   `cache_control` breakpoints and read no `Message.metadata`. Every turn
   re-sent the transcript uncached. On a coding session this is not a
   nice-to-have: it is the difference between viable and unviable cost.

   **Resolution.** The Anthropic driver places up to four `cache_control`
   breakpoints per request — the tool array, the system prompt, and the last
   *two* messages; the pair is what makes caching incremental, since each turn
   reads the cache the previous turn wrote. On by default,
   `prompt_caching(false)` to disable for a proxy that rejects the field.
   Cache-creation and cache-read tokens are summed into `Usage::input_tokens`
   so enabling it does not make a session look like it stopped sending
   context. OpenAI needs nothing: it caches automatically.
4. ✅ **`ModelSpec` cannot express a subscription provider — fixed** (core).

   *Was:* it carried
   `api_key` and `base_url`. yolop's Codex provider needs a refresh token, an
   account id, and an expiry; Google/Ollama/custom endpoints work only because
   they reduce to a base URL. everruns' `ProviderMetadata` was the shape that was
   missing — or, in the spirit of
   [`extensibility.md`](../extensibility/extensibility.md), a `metadata` hatch on `ModelSpec`,
   since the contents are provider-flavored rather than universal.

   **Resolution.** `ModelSpec::metadata`, the hatch — since superseded for
   the credential half of the problem. A `Provider` now owns the endpoint and
   a per-request `ProviderAuth`, so a Codex refresh token belongs there
   (where it can actually be refreshed mid-session) rather than in a spec
   field. `metadata` remains for model-flavored request knobs, and a
   `ModelSpec` carries no secret at all. See
   [`providers.md`](../extensibility/providers.md).

## Sharp edges

Real, worked around, but each one costs an adopter something.

5. ✅ **The filesystem capability is a starter set — fixed.**

   *Was:* no `edit_file` (the
   content-hash CAS everruns has), no `grep_files`, no `stat_file`, no
   offset/limit reads, no byte caps. The backend wrote `edit_file` and
   `grep_files` itself; neither is yolop-specific and both belong in the
   bundled capability. A coding agent without a targeted-edit tool rewrites
   whole files, and one without repository search shells out for every lookup —
   which routes around the approval gate that the shell, unlike the file tools,
   is subject to.

   **Resolution.** `edit_file` (exact-string replacement, refused when the
   match is ambiguous — the same guarantee everruns gets from a content hash,
   in terms a model can act on), `grep_files` (regex, recursive, bounded), and
   `stat_file` now ship, plus `offset`/`limit` line windows on `read_file`.
   All are written against the `FileSystem` trait, so they work over any
   store; `FileSystem::stat` is defaulted in terms of `list_directory` so
   existing implementations gain it for free. Every bundled tool also declares
   risk hints in `ToolDefinition.metadata`, so approval gates on what a tool
   says about itself.
6. ✅ **Tool batches run sequentially — fixed.**

   *Was:* the data model was parallel-capable (`PendingAct { calls }`) and
   `TurnEngine` prepared the whole batch, but `InProcessExecutor` dispatched
   one at a time.

   **Resolution.** The in-process host runs the batch concurrently through
   `concurrency::join_all` — std-only, in core, because taking a `futures`
   dependency to poll N futures would put a combinator library in the leanest
   crate in the workspace. Results are recorded in batch order regardless of
   completion order, so two replays of a session agree. The cost is that
   policy can no longer stop a batch mid-flight, so
   `InProcessExecutor::sequential()` keeps the old behavior for hosts whose
   tools contend or whose budget must bite inside a batch.
7. ✅ **No mid-turn input — fixed.**

   *Was:* `Session::run` borrowed `&mut self` for the whole turn, and there
   was no way to append a message to history without running one.

   **Resolution.** `Session::input()` returns an `InputQueue` a caller holds
   before starting a turn and pushes to from anywhere. The engine drains it
   **only when the next action is a reasoning step** — a message between a
   tool call and its result invalidates the exchange for every provider, and a
   reasoning step is the first moment the model could act on it anyway.
   Drained messages are recorded as ordinary `input.message` events, so
   history and a replay agree, and anything pushed while idle joins the next
   turn.
8. ✅ **MCP is stdio-only and unauthenticated — fixed.**

   *Was:* no HTTP/SSE transport and no auth seam, so remote MCP servers could
   not be reached at all.

   **Resolution.** `McpServer::http` speaks the Streamable HTTP transport:
   one endpoint, JSON *or* SSE responses, and the session id from `initialize`
   echoed on everything after. Splitting a `Transport` trait out first is what
   kept the protocol layer — handshake, `tools/list`, `tools/call`,
   id correlation — shared rather than duplicated per transport.
   `McpAuthProvider` supplies the `Authorization` header **per request**, not
   per connection, so an expiring token only has to return a fresh value.
   The optional `mcp-oauth` feature adopts yolop's protocol layer: RFC 9728 /
   RFC 8414 discovery, client identification (pre-registration, then a Client
   ID Metadata Document, then the deprecated dynamic registration), PKCE
   loopback login with RFC 9207 `iss` validation, code exchange, and
   serialized token refresh. The reusable library stops at the authorization
   URL and serializable tokens; opening a browser, hosting a metadata
   document, and choosing an issuer-keyed credential store belong to the host.
   Protocol handling now has the same multi-era policy as `everruns-mcp`, on
   both transports: `Auto` reaches for the stateless `2026-07-28` protocol and
   falls back to the server-negotiated stateful era only on an explicit
   signal — a `-32022` naming versions we can speak, or, failing that, a body
   that complains about sessions. HTTP learns this from the first real
   request; stdio, which has no status codes, probes with `server/discover`.
   Pinned `Latest`/`Stateful`/`Legacy` modes skip the probe. Because one
   `McpClient` owns one logical connection, its negotiation verdict — and its
   `ttlMs`-bounded `tools/list` cache — live with that client rather than in
   everruns' shared, credential-keyed transport cache.
   Needs the `http` feature alongside `mcp`; without it, connecting says so.
   Live activation followed: `DynamicMcpCapability` owns a host-reloadable set
   of these per-server capabilities. `tools()` snapshots it during each turn's
   assembly, so Yolop can activate, deactivate, or replace servers without
   mutating `Agent`; an in-flight turn retains its old client snapshot.
9. ✅ **Reasoning effort is unvalidated — fixed.**

   *Was:* `ModelSpec::reasoning_effort` took any string, with no model-profile
   notion, so an unsupported effort surfaced as a provider error mid-turn.

   **Resolution.** `ModelProfile` + the `ModelCatalog` seam, validated by
   `AgentBuilder::build()`. The deferral's worry — "a catalog is a subsystem,
   and a wrong one is worse than none" — is answered by shipping **no model
   list at all**: a host implements the trait over knowledge it already has,
   an unknown model passes through untouched, and the library contributes only
   the check and the error message.
10. **Middleware has no host channel.** A UI that must *ask* a human has to
    capture its own channel when the middleware is constructed, or fish one out
    of `ToolContext::extensions`. That works, and may be the right answer — but
    it is worth deciding deliberately rather than by omission, because every
    adopter with a UI hits it.

## Found while consuming the fixes

Adopting the five in yolop turned up one more, small enough to fix in the same
pass: **`Session::run` took `impl Into<String>`**, so a turn could not be
*opened* with an image even though `Message` and every driver already carried
one — the tool-result half of multimodal had landed while the input half was
unreachable. `run` now takes `impl Into<Message>`; `run("hello")` is
unchanged. The lesson is worth keeping: a capability added at one end of a
pipeline is not usable until every entry point admits it, and only an adopter
notices.

## Found by running it for real

Everything above was proven offline. The first **live provider runs** — real
Anthropic calls, a hosted MCP server, a real image — turned up two things no
amount of `SimDriver` coverage would have:

- **The drivers could not talk to anything through an inspecting proxy.**
  `reqwest` was built with the bundled public roots only, so a connection
  terminated by a private CA failed verification and surfaced as "error
  sending request" — while `curl` on the same machine succeeded, because it
  reads the system trust store. Fixed by trusting both root sets. This is the
  kind of defect that is invisible to a test suite and total to an adopter:
  every provider, every call, no useful diagnostic.
- **Prompt caching works, and now there is proof.** The exact breakpoint shape
  the driver emits produced a 4207-token cache *write* on the first request and
  a 4207-token cache *read* on the second, against the real API. Worth noting
  what that also confirms: `input_tokens` alone reported **3** for a request
  that really sent 4210, which is why `Usage` sums the cache fields.

The live runs also confirmed, end to end against real providers: the filesystem
tools, the sandboxed shell with progress narration, concurrent batch dispatch
(two shell calls whose output interleaved), an image opening a turn, MCP over
HTTP against GitHub's hosted server with a bearer token, and a model catalog
rejecting an unsupported reasoning effort before any request left the process.

One adopter-side bug surfaced too, and it belongs in yolop rather than here: a
capability whose `tools()` fails aborts the turn, so a single MCP server
missing its token took down the whole run. Worth a note for the library
regardless — a host has no way to distinguish "one capability is unavailable"
from "the turn failed" other than the error string, and no event marks it.

## Follow-up production parity

Three later gaps were resolved at the boundary where each belongs:

- **Dynamic MCP** is a mutable capability resource, not a mutable agent.
  `DynamicMcpCapability` snapshots its active server set once per turn
  assembly. Removed or replaced clients stay alive for calls already in
  flight; the next turn sees the new definitions.
- **Production persistence** already fits `EventStore`. Yolop should adapt its
  existing locked, fsynced, private, tail-repairing session log and project an
  effective branch through `read_page`; `JsonlEventLog` is explicitly a local
  single-process store rather than the production bar. No second storage
  abstraction is needed.
- **Narration** is authored by `Tool::narrate` at `Started`, `Completed`, or
  `Failed`, after call rewrites, and captured with `display_name` on durable
  lifecycle events. A replaying UI reads the same text the live UI saw instead
  of reconstructing it from a reduced event shape.

## What is left

Gaps 1–9 are closed. What remains is one open question and a short list of
things this analysis never claimed:

- **10, a host channel for middleware.** Still open, and now a smaller
  question than it looked: middleware reaches `ToolContext`, which carries
  extensions and the cancellation token, so a host channel can be injected
  today. Whether a *first-class* one is worth a core field is a decision to
  make on evidence, not by omission.
- **Byte caps on `read_file`** (line windows exist) and
  **background/detached tools** were never in this list; they remain open.

Everything protocol-affecting has landed, which was the ordering constraint:
logs are starting to be persisted.

[yolop-backend]: https://github.com/everruns/yolop/blob/main/knowledge/specs/agentyk-backend.md
