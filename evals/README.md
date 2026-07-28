# evals — agentyk evaluation studies

Agentyk's test suite proves the library does what it says on inputs we choose.
These studies measure what it does when a real model drives it: which tools get
called, what a session costs in tokens and time, whether an approval gate holds,
whether the prompt cache is working.

Each study is a [Mira](https://github.com/everruns/mira) **study** — the generic
`mira` host CLI owns the target matrix, selection, concurrency, saved run folders,
and reporting, while the study owns the cases, the subject, and the scoring. One
study per subfolder.

| Study | What it measures |
|-------|------------------|
| [`agentyk_basic/`](agentyk_basic/) | The library driven as a coding agent: tool use, tool *choice*, approval-gate compliance, and per-session cost — tokens, prompt-cache reuse, tool calls, latency — across models and across the driver's streaming and non-streaming paths. |

The studies live outside the Cargo workspace: they depend on the crates by path
(an eval that scored a published version would grade last release), but they are
not part of what agentyk ships, and they take dependencies the library refuses.

## Running a study

Install the host CLI once (`brew install everruns/tap/mira`, or
`cargo install mira-cli --locked`), then drive a study from its own directory so
its `mira.toml` is found and saved runs land in that study's `results/`:

```bash
cd evals/agentyk_basic
mira list                       # what this study advertises
mira run --preset offline       # no API key, no network, no cost
doppler run -- mira run --preset smoke
doppler run -- mira run         # the whole matrix
```

Provider keys come from Doppler (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`). A
target whose key is missing is **skipped**, not failed, so a keyless run is a
no-op rather than a wall of red — and the `offline` preset is graded either way.

## What an eval is for here

An eval earns its place by measuring something a unit test cannot: behaviour
that depends on a real model's choices, or a number that only exists once a real
provider has answered. Two rules keep the suite honest:

- **A case is data.** Samples carry their own assertions, harness knobs, and (for
  offline cases) the scripted model turns. Adding a case means adding a `Sample`.
- **A finding is a fix.** When a study surfaces a defect in the library, the fix
  lands in the crates with a unit test that fails without it; the eval stays as
  the regression net. See `agentyk_basic/README.md` for the findings so far.
