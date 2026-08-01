# agentyk

Compose agents from values and run them.

`agentyk` is a Rust library for building agents by **composing objects** — a
system prompt, a model, capabilities, tools — and running them. There is no
entity lifecycle: nothing to create in a store, nothing to register, no ids to
thread. Ids exist only as internal correlation handles on sessions and events.

```rust
use agentyk::{Agent, ModelSpec};

let agent = Agent::builder()
    .system_prompt("You are a coding agent.")
    .model(ModelSpec::anthropic("claude-sonnet-4-5").api_key(key))
    .capability(my_capability)
    .build()?;

let mut session = agent.session();
let turn = session.run("list the files").await?;
println!("{}", turn.response);
```

## What's inside

- **Turn loop** — the everruns `input → reason → act` contract: model
  completion, tool execution, repeat until a text answer.
- **Event log** — every step is a typed event (`turn.started`,
  `input.message`, `tool.completed`, …). Logs are pluggable
  (`InMemoryEventLog`, the single-process local `JsonlEventLog`, or your own
  production `EventStore` impl) and sessions resume by replaying them. A host
  store owns cross-process concurrency, fsync/transaction durability, access
  control, tail recovery, and physical branch layout. Bounded pages,
  immutable historical points, forks, and disposable snapshots support
  long-lived timelines.
- **Event listeners** — `EventListener` observes durable and ephemeral events,
  optionally filtered by event type. `CompositeEventListener` combines
  observers with ordered, panic-isolated delivery.
- **Capabilities** — composable extensions contributing system-prompt text and
  tools, attached by object:
  `.capability(FileSystemCapability::new(store))`. The bundled filesystem one
  covers what a coding agent needs: read (whole file or a line window),
  write, targeted `edit_file`, `grep_files`, `stat_file`, list, delete.
- **Cancellation that lands** — a `CancellationToken` stops the turn *and*
  drops the tool call in flight, so cancelling during a long build or test run
  takes effect immediately instead of when the command happens to finish.
- **Model profiles** — attach a `ModelCatalog` and an unsupported reasoning
  effort fails at `build()` instead of as a provider error mid-turn. The
  catalog is a seam; agentyk ships no model list to go stale.
- **Live tool progress** — a running tool calls
  `ToolContext::report_progress`, and the host sees ephemeral `tool.progress`
  events while it works. Results can carry structured `metadata` for the host
  and image `parts` for the model, alongside the text both read. A tool can
  also supply `display_name()` and phase-aware `narrate()` text, durably
  captured on its start and completion events.
- **MCP** — `McpCapability` connects to Model Context Protocol servers over
  stdio or HTTP and exposes their tools to the model. `DynamicMcpCapability`
  lets a host activate, deactivate, or atomically replace a server set; the
  next turn sees the new snapshot without mutating the `Agent` or replacing
  its session log. Both transports default
  to `McpProtocolMode::Auto` and speak the stateless `2026-07-28` protocol
  where they can — carrying the protocol version, client capabilities, and
  identity in each request's `_meta` — falling back to the initialize
  handshake for a server from an earlier revision. HTTP finds out from the
  first request; stdio, which has no status codes to read, probes with
  `server/discover`. `tools/list` is cached for the `ttlMs` the server
  reports. `McpAuthProvider` supplies credentials per request. Feature
  `mcp-oauth` adds OAuth 2.1 discovery, client identification (pre-registered,
  Client ID Metadata Document, or dynamic registration), PKCE browser login
  with RFC 9207 issuer validation, and automatic token refresh; the
  application opens the returned URL and persists tokens.
- **Steering** — `Session::input()` hands out a queue a UI can push to while a
  turn is running; messages join the conversation at its next reasoning step.
- **User hooks** — the six Everruns lifecycle points (`session_start`,
  `user_prompt_submit`, `pre_tool_use`, `post_tool_use`, `turn_end`,
  `session_end`) compose as ordinary `Hook` values. Prompt/tool hooks can
  mutate or block; end hooks are advisory. The optional `hooks` feature adds
  trusted local `ShellHook`s using the same structured decision contract.
- **Multi-actor sessions** — route an addressed turn to another by-value
  `Agent` with `Session::run_with_agent` while retaining the shared replayable
  history. `ExternalActor` distinguishes users from external channels, and
  event metadata carries host-owned participant provenance.
- **Concurrent tools** — a batch the model asked for in parallel runs in
  parallel, with results still recorded in the order it asked.
- **Multi-provider drivers** — `ChatDriver` implementations routed by
  `DriverId`: OpenAI-compatible and Anthropic (feature `http`), plus a
  scripted `SimDriver` for deterministic offline tests and examples. The
  Anthropic driver places prompt-cache breakpoints by default, so a long
  session does not pay full price to re-send its own transcript.

## Multi-actor demo

[`examples/osbb`](examples/osbb) seats five co-owners of an apartment building
in one conversation with the agent that answers for their association: two of
them report the same night noise from different apartments, and the association
answers both on the same session. Each input keeps its named `ExternalActor`,
and the model sees speaker labels without those labels rewriting durable
history.

<img src="examples/osbb/docs/demo.gif" width="880" alt="Olena reports night music from apartment 41, Petro confirms it from another apartment, and the Manager logs the two reports separately, cites quiet hours, and puts the matter on the board agenda.">

<sup>Real `openai/gpt-5.6-terra` run. See the example README for the offline
tests and recording recipe.</sup>

## Background hosting examples

These examples keep task lifecycle in the application, matching the boundary
used by Everruns and Yolop, while Agentyk runs the parent and child turns:

- [`github_monitor.rs`](crates/agentyk/examples/github_monitor.rs) detaches
  `gh pr checks --watch`, ends the foreground turn, and wakes the same session
  when the command finishes. It needs an authenticated `gh` CLI:
  `cargo run -p agentyk --example github_monitor -- OWNER/REPO PR_NUMBER`.
- [`subagents.rs`](crates/agentyk/examples/subagents.rs) starts five independent
  child-agent sessions, returns their task ids, and has the parent wait for all
  five: `cargo run -p agentyk --example subagents`.

## Packaging

Three crates separate portable contracts, canonical turn semantics, and
bundled implementations:

- **`agentyk-core`** — the contract: what you *implement against*. Traits
  and portable values, events, and turn reducers.
- **`agentyk-engine`** — the canonical step engine, `Agent`, `Session`, and
  in-process runner. Everruns durable execution hosts this same engine one
  persisted step at a time.
- **`agentyk`** — the application facade and bundled feature-gated modules:
  drivers, event stores, MCP, and filesystem support.

MCP and filesystem are first-class parts of the library, not separate
integration crates. Other integrations also stay as modules for now. See
[`docs/architecture.md`](docs/architecture.md) for the execution and
durability model.

## Features

`agentyk` ships with **no features on by default**: the bare crate gives you
the turn loop, the event logs, and the offline `SimDriver`, and pulls in
nothing that can open a socket or spawn a process. Opt in to what you need.

| Feature | Adds | Pulls in |
| --- | --- | --- |
| *(none)* | turn loop, `InMemoryEventLog`, `JsonlEventLog`, `SimDriver` | — |
| `http` | `OpenAiDriver`, `AnthropicDriver` (SSE streaming) | `reqwest`, `futures-util` |
| `mcp` | static or live-reloadable MCP capabilities / `McpClient` over stdio (HTTP transport also needs `http`) | `tokio` (rt, process, io-util, sync, time) |
| `mcp-oauth` | OAuth 2.1 discovery, DCR, PKCE loopback login, token refresh | `mcp`, `http`, `base64`, `rand`, `sha2` |
| `fs` | `FileSystemCapability`, real-disk and in-memory stores | `tokio` (fs, sync), `regex` |
| `hooks` | trusted local `ShellHook` executor | `tokio` (process, io-util, time) |
| `full` | all of the above | all of the above |

```toml
agentyk = { version = "0.1", features = ["http", "fs"] }
```

## Hooks

Implement `Hook` for an in-process callback and attach it by value:

```rust
use agentyk::{
    Hook, HookEvent, HookOutcome, HookPayload,
};
use async_trait::async_trait;

struct ProtectDeploy;

#[async_trait]
impl Hook for ProtectDeploy {
    fn id(&self) -> &str { "protect-deploy" }
    fn event(&self) -> HookEvent { HookEvent::PreToolUse }

    async fn run(&self, payload: &HookPayload) -> HookOutcome {
        if payload.data["tool_name"] == "deploy" {
            HookOutcome::Block {
                reason: "deploy requires approval".into(),
                user_message: Some("Approve the deployment first.".into()),
            }
        } else {
            HookOutcome::Allow
        }
    }
}

let agent = Agent::builder()
    // model + driver + tools...
    .hook(ProtectDeploy)
    .build()?;
```

Hooks with the same event run in attachment order. A prompt mutation replaces
`patch.message`; a pre-tool mutation shallow-merges `patch.arguments`; a
post-tool mutation may replace `patch.result` / `patch.error` or append
`patch.additional_context`. The first prompt/pre-tool block stops that action.
`post_tool_use`, `turn_end`, and session lifecycle events are advisory because
the observed side effect has already happened (or has no blockable action).

With feature `hooks`, `ShellHook` runs a trusted local command:

```rust
use agentyk::{HookErrorPolicy, HookEvent, ShellHook};

let lint = ShellHook::new(
    "lint-after-edit",
    HookEvent::PostToolUse,
    "scripts/lint-hook.sh",
)
.on_error(HookErrorPolicy::Warn);
```

The command receives a JSON `HookPayload` on stdin and in
`AGENTYK_HOOK_PAYLOAD_JSON`, plus convenience `AGENTYK_HOOK_*` variables. It
returns JSON such as `{"decision":"allow"}` or
`{"decision":"mutate","patch":{...}}`; empty stdout uses the exit code as a
Git-hook-style allow/block decision. Execution is capped at 30 seconds and
64 KiB of output. `ShellHook` uses `/bin/sh` with the application's OS
permissions—it is deliberately not presented as a sandbox. A server or
durable host should implement `Hook` over its own sandboxed executor.

Because `Agent::session()` is synchronous, `session_start` fires immediately
before the new session's first turn. Async `session_end` hooks fire from the
explicit, idempotent `Session::close().await?`; dropping a session cannot await.
A resumed non-empty session does not replay `session_start`; `session_end`
only runs when that handle is explicitly closed.

A durable host can retry a hook if it crashes after the external command ran
but before the resulting events were committed—the same at-least-once boundary
as a tool call. Side-effecting hooks should use `hook_id` plus session/turn/tool
ids as an idempotency key when duplicate execution matters.

## Try it (offline, no API key)

```sh
cargo run -p agentyk --example hello
cargo test --workspace --all-features
```

## Inspect and fork history

Every durable head is an immutable `SessionPoint`. Inspection is read-only;
forking creates a new session and leaves the original branch intact.

```rust
let point = session.point().await?;
let past = session.inspect(point).await?;
println!("{} messages", past.messages().len());

let mut alternative = session.fork(point).await?;
alternative.run("try a different approach").await?;
```

Forks are accepted at empty or completed-turn boundaries. Mid-turn points are
still inspectable, but cannot be continued as a new branch because doing so
could repeat a partially completed external action.

After a process failure, `session.resume_pending().await?` continues an
incomplete turn only at the current head. Tool execution is at-least-once
across that recovery boundary, so side-effecting tools should use idempotency
keys when a host requires deduplication.

## A real application

[`examples/codenko`](examples/codenko) is a small terminal coding agent —
filesystem tools, a shell tool behind an approval prompt, streaming output,
cancellable turns — in about 1,450 lines. It is the short version of what
building on agentyk looks like: the whole UI is a fold over the event stream,
so it is tested without a terminal or a network.

```sh
ANTHROPIC_API_KEY=... cargo run --release -p codenko -- --dir path/to/project
```

## Relationship to everruns

The domain language — events protocol, capabilities, drivers, the turn
contract — is inherited from [everruns](https://github.com/everruns/everruns).
agentyk is the value-first core; the plan is to rebuild `everruns-core` and
`everruns-runtime` on top of it, with identity, persistence, and multitenancy
layered on by hosts rather than baked into the model. See
[`knowledge/plan.md`](knowledge/plan.md).

## License

MIT
