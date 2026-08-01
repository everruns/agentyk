---
type: Design
title: User hook lifecycle
description: >-
  Defines Agentyk’s Everruns-compatible hook events, composition semantics,
  and executor boundary.
tags: [hooks, lifecycle, middleware, extensibility]
---

# User hook lifecycle

Agentyk exposes the six user-facing hook events supported by Everruns:
`session_start`, `user_prompt_submit`, `pre_tool_use`, `post_tool_use`,
`turn_end`, and `session_end`. They share one host-neutral `Hook` trait and
structured `HookPayload` / `HookOutcome` values. One trait keeps new executor
backends and hook contributors composable without adding a field and trait
family for every interception point.

## Semantics

Hooks select one event and run in agent attachment order within that event.
Tool-event matchers support exact or restricted-glob tool names, a dot-only
JSON argument path, and a Rust regular expression.

- `user_prompt_submit` may replace the user message or block the turn. A block
  records `turn.failed` and returns `Error::HookBlocked`.
- `pre_tool_use` may shallow-merge tool arguments or block that individual
  call. A mutation is recorded as `tool.rewritten`; a block records
  `tool.denied`, then a normal error `tool.completed` result so the model can
  react.
- `post_tool_use` may replace the tool result/error or append model-visible
  context. It cannot undo a completed side effect, so a block is advisory.
- `session_start`, `turn_end`, and `session_end` are advisory. Executor errors
  obey each hook's allow/warn policy but cannot reverse the lifecycle event.

An executor error policy of `block` is preventive only at prompt and pre-tool
events. At post-tool use it converts the result into a model-visible error;
at lifecycle/end events it remains advisory. `warn` produces a durable
`hook.warning` custom event when a turn exists.

Rust `TurnMiddleware` remains the typed policy seam for programmatic
guardrails. User pre-tool hooks run before middleware, and post-tool hooks run
after middleware, so middleware can apply trusted in-process policy around
untrusted/external hook behavior. Both are orchestrated by the canonical
engine; neither owns a copied turn loop.

## Session boundary

Agentyk sessions are in-memory values, not asynchronously created database
records. `Agent::session()` therefore cannot await `session_start`: the event
fires immediately before the new session's first turn. `session_end` fires
from explicit, idempotent `Session::close().await`; Rust `Drop` cannot await.
A resumed non-empty session marks start as already handled. Session end only
runs when the resumed handle is explicitly closed.

Durable execution is at-least-once across a crash between hook execution and
effect commit, matching tool execution. Hook authors use the stable hook,
session, turn, and tool-call ids as idempotency keys when their command has an
external side effect.

## Executor boundary

The contract crate contains data and the `Hook` trait only. It never spawns a
process. The facade's optional `hooks` feature supplies `ShellHook` for trusted
local applications, with JSON on stdin/environment, structured decisions, a
30-second timeout ceiling, and a 64 KiB output ceiling.

`ShellHook` runs `/bin/sh` with the application's OS authority and is not a
sandbox. Everruns can implement the same `Hook` contract using its VFS,
egress policy, and durable/sandboxed executor. This preserves the packaging
rule: third-party-implemented contracts live in core; process execution stays
in a feature-gated bundled integration or adopting host.
