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
   **Still open:** multimodal tool results *to the model* (an image a model can
   look at). That needs `ToolOutput` to become content parts plus per-driver
   support in `tool_result` blocks, and only one of the two bundled providers
   accepts them — worth doing on evidence of need, not speculatively.
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
   [`extensibility.md`](extensibility.md), a `metadata` hatch on `ModelSpec`,
   since the contents are provider-flavored rather than universal.

   **Resolution.** `ModelSpec::metadata`, the hatch. It is the natural home
   for credentials, so it is redacted in `Debug` alongside `api_key` and falls
   under the same rule: a `ModelSpec` never reaches an event or a log line.

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
6. **Tool batches run sequentially.** The data model is parallel-capable
   (`PendingAct { calls }`) and `TurnEngine` prepares the whole batch, but
   `InProcessExecutor` dispatches one at a time. Parallel reads are table
   stakes; this is the follow-up
   [`everruns-adoption.md`](everruns-adoption.md) gap 8 already names.
7. **No mid-turn input.** `Session::run` borrows `&mut self` for the whole
   turn, and there is no way to append a message to history without running
   one. Steering ("stop, do X instead") and injecting a system notice mid-turn
   are both unexpressible. A queue the host drains between actions, or an
   `append_message`-style API, would cover it.
8. **MCP is stdio-only and unauthenticated.** No HTTP/SSE transport and no auth
   seam, so remote MCP servers cannot be reached at all. Capabilities still
   cannot contribute servers (gap 13).
9. **Reasoning effort is unvalidated.** `ModelSpec::reasoning_effort` takes any
   string. There is no model-profile notion (context window, supported efforts,
   max tokens), so an unsupported effort surfaces as a provider error mid-turn
   instead of at build time.
10. **Middleware has no host channel.** A UI that must *ask* a human has to
    capture its own channel when the middleware is constructed, or fish one out
    of `ToolContext::extensions`. That works, and may be the right answer — but
    it is worth deciding deliberately rather than by omission, because every
    adopter with a UI hits it.

## What is left

Gaps 1–5 are closed. The rest stay open, deliberately:

- **6, concurrent dispatch** — the data model is already parallel-capable, so
  this is a host change, not a protocol one. It also interacts with the
  per-call cancellation and budget guards, which is worth designing rather
  than bolting on.
- **7, mid-turn input** — needs a decision about what a message appended
  *during* a turn means for replay, not just an API.
- **8, MCP transports** — an HTTP/SSE client plus an auth seam; a chunk of
  work in its own right.
- **9, model profiles** — a catalog of what each model supports is a
  subsystem, and a wrong one is worse than none.
- **10, a host channel for middleware** — capturing a channel in the
  middleware works today. Worth deciding deliberately, not by omission.

Everything protocol-affecting in this analysis has landed, which was the
ordering constraint: logs are starting to be persisted.

[yolop-backend]: https://github.com/everruns/yolop/blob/main/knowledge/specs/agentyk-backend.md
