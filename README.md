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
  sessions resume by replaying them.
- **Capabilities** — composable extensions contributing system-prompt text and
  tools, attached by object:
  `.capability(FileSystemCapability::new(store))`.
- **MCP** — `McpCapability` connects to Model Context Protocol servers over
  stdio and exposes their tools to the model.
- **Multi-provider drivers** — `ChatDriver` implementations routed by
  `DriverId`: OpenAI-compatible and Anthropic (feature `http`), plus a
  scripted `SimDriver` for deterministic offline tests and examples.

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
| `mcp` | `McpCapability` / `McpClient` over stdio | `tokio` (rt, process, io-util, sync, time) |
| `fs` | `FileSystemCapability`, real-disk and in-memory stores | `tokio` (fs, sync) |
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
