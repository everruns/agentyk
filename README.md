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
  tools, attached by object: `.capability(FileSystem::new())`.
- **MCP** — `McpCapability` connects to Model Context Protocol servers over
  stdio and exposes their tools to the model.
- **Multi-provider drivers** — `ChatDriver` implementations routed by
  `DriverId`: OpenAI-compatible and Anthropic (feature `http`), plus a
  scripted `SimDriver` for deterministic offline tests and examples.

## Packaging

Two crates, lockstep-versioned:

- **`agentyk`** — the framework: what you *build with*. `Agent`/`Session`
  builders, the default in-process executor, bundled drivers, the JSONL event
  log, MCP. Re-exports all of core, so this is the only dependency an
  application needs.
- **`agentyk-core`** — the contract: what you *implement against*. Traits
  (`Tool`, `Capability`, `ChatDriver`, `EventLog`, `TurnExecutor`), the event
  protocol, and the turn state machine. Deliberately lean — no tokio, no
  HTTP — so hosts (a durable engine, a server) and extensions depend on a
  small, stable surface.

New drivers and capabilities start as feature-gated modules in `agentyk`; a
module graduates to its own `agentyk-<name>` crate (depending only on core)
when it grows a heavy dependency.

## Try it (offline, no API key)

```sh
cargo run -p agentyk --example hello
cargo test --workspace --all-features
```

## Relationship to everruns

The domain language — events protocol, capabilities, drivers, the turn
contract — is inherited from [everruns](https://github.com/everruns/everruns).
agentyk is the value-first core; the plan is to rebuild `everruns-core` and
`everruns-runtime` on top of it, with identity, persistence, and multitenancy
layered on by hosts rather than baked into the model. See
[`docs/plan.md`](docs/plan.md).

## License

MIT
