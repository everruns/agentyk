---
type: Analysis
title: Yolop adoption — what a real coding agent hits
description: Gaps found by porting yolop's execution story onto agentyk's seams, ordered by how hard they block a shipping coding agent.
tags: [adoption, yolop, gaps, drivers, tools, extensibility]
timestamp: 2026-07-25
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
[`knowledge/specs/agentyk-backend.md`](https://github.com/everruns/yolop/blob/main/knowledge/specs/agentyk-backend.md)
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
shipping on agentyk.

1. **A tool call cannot be cancelled** (engine). `InProcessExecutor` checks the
   cancellation token between actions and between streaming chunks, but awaits
   `atoms::act` unraced, and `ToolContext` carries no token. Ctrl-c during a
   two-minute `cargo test` is honored only when the command finishes. The fix
   is two-part and both parts are needed: race the tool future against the
   token in the host, **and** put the token on `ToolContext` so a tool can
   cooperate (kill its child rather than be abandoned). The same applies to a
   middleware awaiting an approval answer — a turn cancelled while a prompt is
   open cannot currently unwind.
2. **Tools cannot report progress or return anything but a string** (core +
   engine). `ToolOutput` is `{ content: String, is_error: bool }` and
   `ToolContext` has no event sink. So: no streaming command output, no
   narration during a call, no background/long-running tools, no image or
   structured results. everruns' `BackgroundEventSink` and narration phases
   have no counterpart. A sink on `ToolContext` emitting `EventData::Custom`
   would unblock most of it without a protocol change; rich results need a
   content-part-shaped `ToolOutput`, which is a protocol change and therefore
   cheaper now than later.
3. **No prompt caching** (drivers). The Anthropic driver emits no
   `cache_control` breakpoints and reads no `Message.metadata`. Every turn
   re-sends the transcript uncached. On a coding session this is not a
   nice-to-have: it is the difference between viable and unviable cost.
4. **`ModelSpec` cannot express a subscription provider** (core). It carries
   `api_key` and `base_url`. yolop's Codex provider needs a refresh token, an
   account id, and an expiry; Google/Ollama/custom endpoints work only because
   they reduce to a base URL. everruns' `ProviderMetadata` is the shape that is
   missing — or, in the spirit of
   [`extensibility.md`](extensibility.md), a `metadata` hatch on `ModelSpec`,
   since the contents are provider-flavored rather than universal.

## Sharp edges

Real, worked around, but each one costs an adopter something.

5. **The filesystem capability is a starter set.** No `edit_file` (the
   content-hash CAS everruns has), no `grep_files`, no `stat_file`, no
   offset/limit reads, no byte caps. The backend wrote `edit_file` and
   `grep_files` itself; neither is yolop-specific and both belong in the
   bundled capability. A coding agent without a targeted-edit tool rewrites
   whole files, and one without repository search shells out for every lookup —
   which routes around the approval gate that the shell, unlike the file tools,
   is subject to.
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

## Suggested order

Protocol-affecting first, since logs are starting to be persisted:

1. `ToolOutput` content parts + a tool event sink on `ToolContext` (gap 2).
2. `ModelSpec` provider metadata hatch (gap 4).
3. Cancellation through tool execution (gap 1) — engine-only, no protocol
   change, but the most user-visible.
4. Prompt caching in the Anthropic driver (gap 3).
5. `edit_file` / `grep_files` / `stat_file` in `FileSystemCapability` (gap 5)
   and concurrent dispatch in `InProcessExecutor` (gap 6).

Gaps 7–10 can follow adoption; each is additive.
