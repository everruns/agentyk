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
  (`InMemoryEventLog`, `JsonlEventLog`, or your own `EventLog` impl) and
  sessions resume by replaying them. Bounded pages, immutable historical
  points, forks, and disposable snapshots support long-lived timelines.
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
  and image `parts` for the model, alongside the text both read.
- **MCP** — `McpCapability` connects to Model Context Protocol servers over
  stdio or HTTP and exposes their tools to the model. `McpAuthProvider`
  supplies a remote server's credentials per request, so an expiring token is
  a matter of returning a fresh value.
- **Steering** — `Session::input()` hands out a queue a UI can push to while a
  turn is running; messages join the conversation at its next reasoning step.
- **Concurrent tools** — a batch the model asked for in parallel runs in
  parallel, with results still recorded in the order it asked.
- **Multi-provider drivers** — `ChatDriver` implementations routed by
  `DriverId`: OpenAI-compatible and Anthropic (feature `http`), plus a
  scripted `SimDriver` for deterministic offline tests and examples. The
  Anthropic driver places prompt-cache breakpoints by default, so a long
  session does not pay full price to re-send its own transcript.

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
| `mcp` | `McpCapability` / `McpClient` over stdio (HTTP transport also needs `http`) | `tokio` (rt, process, io-util, sync, time) |
| `fs` | `FileSystemCapability`, real-disk and in-memory stores | `tokio` (fs, sync), `regex` |
| `full` | all of the above | all of the above |

```toml
agentyk = { version = "0.1", features = ["http", "fs"] }
```

For scale: the default build resolves 28 crates, `full` resolves 100.

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
