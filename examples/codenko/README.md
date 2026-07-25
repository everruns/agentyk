# codenko

A small terminal coding agent: one agent, four tools, one screen. It reads and
writes files in a workspace directory, runs shell commands, and asks before it
changes anything.

codenko exists to show what composing an [`agentyk`](../../crates/agentyk)
agent looks like in a real application, and to be short enough to read in one
sitting — about 1,450 lines of source and 280 of tests. The UI is
[`tuika`](https://crates.io/crates/tuika).

![codenko fixing a bug](docs/demo.gif)

## Run it

```sh
export ANTHROPIC_API_KEY=...          # or OPENAI_API_KEY
cargo run --release -p codenko -- --dir path/to/project
```

With no flags it works in the current directory and picks whichever provider's
key is set:

```
codenko — a small terminal coding agent

Usage: codenko [options]

Options:
  -C, --dir <path>       Workspace directory the agent works in (default: .)
      --provider <name>  anthropic | openai (default: whichever key is set)
      --model <id>       Model id (default: per provider, see below)
      --base-url <url>   Override the provider endpoint (compatible servers,
                         gateways, local runtimes)
      --reasoning-effort <level>
                         Reasoning effort, where the driver supports it
                         (openai: none | low | medium | high)
      --log <path>       Append the session's JSONL event log here
  -h, --help             Show this message

Environment:
  ANTHROPIC_API_KEY      Enables --provider anthropic (default claude-sonnet-5)
  OPENAI_API_KEY         Enables --provider openai (default gpt-5.5)
  ANTHROPIC_BASE_URL     Default for --base-url on --provider anthropic
  OPENAI_BASE_URL        Default for --base-url on --provider openai
```

### Providers

Either driver works; the whole difference is a `ModelSpec`, so switching is a
flag rather than a code path:

```sh
codenko --provider anthropic --model claude-opus-5
codenko --provider openai    --model gpt-5.5
codenko --provider openai    --model gpt-5.6-terra --reasoning-effort none
```

`--reasoning-effort none` is required on that last one, and it is worth knowing
why: OpenAI's `gpt-5.6` family refuses function tools on chat completions at any
other effort level, and codenko is nothing but function tools. Leave the flag
off and the first turn ends in a `turn failed` notice quoting the API's own
explanation — the driver surfaces the response body rather than swallowing it.

![codenko on gpt-5.6-terra](docs/demo-openai.gif)

### Keys

| Key           | Does                                                     |
| ------------- | -------------------------------------------------------- |
| `enter`       | Send the prompt                                          |
| `esc`         | Interrupt the running turn; clears the composer when idle |
| `y` / `n`     | Allow or deny a pending tool call                        |
| `pgup`/`pgdn` | Scroll the transcript (mouse wheel works too)            |
| `ctrl+c`      | Quit                                                     |

The composer is a single line, with the usual readline editing (`ctrl+a`,
`ctrl+e`, `ctrl+w`, `alt+b`/`alt+f`, …) courtesy of tuika's `TextInputState`.

## What the agent can do

Four tools, three of which are gated behind approval:

| Tool             | From                       | Gated |
| ---------------- | -------------------------- | ----- |
| `read_file`      | `FileSystemCapability`     | no    |
| `list_directory` | `FileSystemCapability`     | no    |
| `write_file`     | `FileSystemCapability`     | yes   |
| `delete_file`    | `FileSystemCapability`     | yes   |
| `run_command`    | codenko (`bash -lc`)       | yes   |

The filesystem tools come from agentyk's bundled capability over a
`RealDiskFileSystem` rooted at the workspace, which rejects `..` structurally —
the model cannot escape the root no matter what path it sends. `run_command` is
codenko's own: a bash shell in the workspace, with a 60s timeout and truncated
output so one runaway command can't take the context window or the UI hostage.

Read-only tools are never gated. An agent that asks permission to look at a
file is unusable, and the safety property that matters is on the mutating side.

## How it fits together

```
┌──────────────────────────┐   Prompt (text + CancellationToken)   ┌────────────────┐
│  ui.rs  App              │ ────────────────────────────────────▶ │  agent task    │
│  transcript · composer   │                                       │  owns Session  │
│  scroll · approval       │ ◀──────────────────────────────────── │  runs turns    │
└──────────────────────────┘   AppEvent: agentyk Event  · outcome   └────────────────┘
                                         · approval request
```

A `Session` is `&mut self` for the length of a whole turn, so it lives alone in
its own task and never blocks a frame. Two channels connect the halves, and the
UI's entire model of the conversation is a fold over the agentyk event stream:

- **[`transcript.rs`](src/transcript.rs)** — `Transcript::apply(&Event)` is the
  only way an entry appears. The UI does not append the user's message on
  submit or guess when a tool finished; `input.message`, `tool.started`,
  `tool.completed`, and `tool.denied` do that. Streaming text grows one entry in
  place from ephemeral `output.message.delta` events.
- **[`agent.rs`](src/agent.rs)** — composition (`build_agent`), the shell tool,
  the approval hook, and the session task.
- **[`ui.rs`](src/ui.rs)** — the `tuika` view tree and the update loop.
- **[`config.rs`](src/config.rs)** — flags and provider resolution.

Because the display is a pure function of protocol events, it is tested without
a terminal and without a network: [`tests/transcript.rs`](tests/transcript.rs)
runs real turns through the real executor with the scripted `SimDriver` and
asserts on what the operator would be looking at.

```sh
cargo test -p codenko
```

### Approval, in three moving parts

The gate is agentyk's `PreToolUseHook`, which runs before a tool executes and
whose first `Deny` wins:

1. The hook sends the pending `ToolCall` to the UI over the same channel the
   events use, with a `oneshot` sender for the answer, and awaits it.
2. The UI shows the call in place of the composer; `y`/`n` answers the oneshot.
3. `Allow` runs the tool. `Deny` never runs it — the reason becomes the tool
   result the model sees, plus a `tool.denied` event in the log.

Because the hook simply awaits, the turn is genuinely paused: no state machine,
and nothing to unwind. If the UI quits mid-prompt the sender drops and the hook
fails closed.

### Cancellation

Each prompt carries its own `CancellationToken`. `esc` cancels the clone the UI
kept, and the executor stops at its next check point — between reason and tool
steps, or between streaming chunks. The session survives, so the next prompt
continues the same conversation.

### The event log

`--log session.jsonl` swaps the in-memory log for `JsonlEventLog`. That file is
the persistence seam, not a debug artifact: replaying it reconstructs the
message history, which is what `Agent::resume_session` does.

## Re-recording the demo

Both GIFs are real runs against real models, recorded with
[vhs](https://github.com/charmbracelet/vhs):

```sh
cargo build --release -p codenko
export PATH="$PWD/target/release:$PATH"

ANTHROPIC_API_KEY=... vhs examples/codenko/demo.tape          # docs/demo.gif
OPENAI_API_KEY=...    vhs examples/codenko/demo-openai.tape   # docs/demo-openai.gif
```

Both tapes run the same scenario — a scratch workspace with a small bug, a
question about it, then the fix — so the recordings exercise reading, writing,
approval, and a verification command, and are directly comparable across
providers. Model wording varies between takes; the sleeps are sized for the
slowest step.

## Deliberate omissions

This is an example, so it stops where the interesting part is over:

- No diffs on `write_file` — the approval prompt shows the call, not a patch.
- One approval at a time, and no "always allow" memory.
- Single-line composer, no history, no slash commands (agentyk capabilities can
  expose commands — see `Session::commands`).
- No context compaction. Long sessions eventually hit the model's window;
  agentyk's `ContextAssembler` is the seam for that.
