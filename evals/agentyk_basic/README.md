# agentyk_basic — the library, driven

A [Mira](https://github.com/everruns/mira) study on the `mira-eval` Rust SDK.
The subject is an **in-process `agentyk` session**, composed the way a coding
agent composes one: the filesystem capability, a shell tool, an approval
middleware, an event log, a real provider driver. What it grades is therefore
the library's own behaviour — turn loop, tool surface, middleware, usage
accounting — not a downstream binary's prompt.

```bash
mira list                        # evals, samples, scorers, targets
mira run --preset offline        # no key, no network, no cost
doppler run -- mira run --preset smoke
doppler run -- mira run --preset cache --group-by target
doppler run -- mira run --preset stream-parity --group-by mode
doppler run -- mira run          # the whole matrix
```

## The evals

| Eval | Asks |
|------|------|
| `coding` | Can it do ordinary work — add a function, fix an off-by-one, find a constant, rename a type across files — through the tool surface? |
| `tool_policy` | Does it use the tools the host *meant*: the file tools over the shell, no shell workaround when a write is declined, and an ordinary edit with no shell at all? |
| `session_efficiency` | What does a multi-turn session cost — tokens, prompt-cache reuse, wall clock — when later turns re-read the same context? |
| `offline_contract` | Do the capability, middleware, event, and trajectory wiring hold on the scripted `sim` model? Runs in CI with no credentials. |

## The matrix

- **targets** — `anthropic/claude-sonnet-5`, `anthropic/claude-haiku-4-5`,
  `openai/gpt-5.5`, `openai/gpt-5.6-terra`; `sim` for the offline eval. A target
  whose key is missing is skipped, not failed, and a target may carry a
  `reasoning_effort` in its metadata when the model needs one.
- **mode** (`coding`) — `stream` vs `buffered`. The engine only ever calls
  `complete_streaming`, so `buffered` wraps the driver to answer from `complete`
  instead. Same cases, same scorers: a difference in pass rate between the two
  is a difference between the provider code paths, which is otherwise invisible
  until a user finds it.

## What every case reports

Beyond pass/fail, each case carries the numbers that make runs comparable:

| Metric | Meaning |
|--------|---------|
| `input_tokens` / `output_tokens` | Totals across every completion in every turn. |
| `cache_read_tokens` / `cache_write_tokens` | The prompt-cache split — subsets of `input_tokens`, priced very differently. |
| `cache_hit_ratio` | Share of prompt tokens served from cache. The headline caching number. |
| `reasoning_tokens` | Thinking tokens, where the provider reports them. |
| `cost_usd` | Estimated, pricing the cache split at its own rate (see `price_for` — approximate by construction). |
| `duration_ms`, time-to-first-token | Wall clock, and the latency a user feels first. |
| `tool_errors`, `denied_tool_calls`, `events` | What the event stream saw. |

Tool calls are reported as an **ATIF trajectory** projected from the agentyk
event stream, so arguments and observations — not just tool names — are
scorable, and the `trajectory` scorer checks that every tool call still has its
observation joined to it by call id.

## Adding a case

Cases are data. Add a `Sample` in `src/cases.rs` with its own assertions and
harness knobs; no scorer or harness change is needed.

```rust
Sample::new("id", "the prompt")
    .file("src/lib.rs", "// seeded into a fresh workspace\n")
    .meta("shell", json!(false))              // withhold run_command
    .meta("deny", json!(["write_file"]))      // the approval gate declines these
    .meta("required_tools", json!(["read_file"]))
    .meta("forbidden_tools", json!(["run_command"]))
    .meta("max_tokens", json!(120_000))
    .meta("min_cache_hit_ratio", json!(0.2))
    .meta("expect_metrics", json!({"denied_tool_calls": 1}))
    .meta("checks", json!([
        {"file": "src/lib.rs", "contains": ["fn greet"], "lacks": ["todo!"]},
        {"response_contains": ["DONE"]},
    ]))
```

An offline (`sim`) case adds `script`, the completions the simulated model
plays: `[{"tool": "read_file", "args": {…}}, {"text": "…"}]`.

## Saved runs

`results/` holds committed run folders (`report.html` is the transcript viewer;
`mira report <run_id>` re-renders one). Runs are kept because a study's numbers
are only meaningful against the run they replaced.

The current baseline — `results/20260728T231856Z-a572`, the whole matrix, **54
passed / 54 scored**, $0.39 total:

| Target | Cases | Cost | Tokens | Cache reuse | Mean latency |
|--------|-------|------|--------|-------------|--------------|
| `sim` | 2/2 | $0 | 0 | — | 1 ms |
| `anthropic/claude-sonnet-5` | 13/13 | $0.153 | 117 469 | 82% | 5.5 s |
| `anthropic/claude-haiku-4-5` | 13/13 | $0.109 | 108 463 | 17% | 3.9 s |
| `openai/gpt-5.5` | 13/13 | $0.071 | 66 058 | 38% | 5.0 s |
| `openai/gpt-5.6-terra` | 13/13 | $0.058 | 60 401 | 46% | 3.7 s |

Cache reuse is the share of *all* prompt tokens served from cache, so it is
dragged down by the short cases that never reach a provider's minimum cacheable
prompt — Haiku's 2048-token floor is why its overall figure is lowest. On the
`repeated-context` session, which clears every floor, all four live targets
reuse 63–79% of their prompt.

`gpt-5.6-terra` carries `reasoning_effort = none` in its target metadata: the
`gpt-5.6` family refuses function tools on chat completions at any other level,
and this study is nothing but function tools.

## Findings

What this study has surfaced in the library so far, each fixed in the crates
with a unit test that fails without the fix:

- **Cached prompt tokens were dropped on the streaming path.** The Anthropic
  driver places `cache_control` breakpoints, but `AnthropicStream` read only
  `input_tokens` from `message_start` and ignored the two cache fields beside
  it. The engine always takes the streaming path, so in practice every cache
  hit under-counted the prompt — the non-streaming path summed them, and the
  two disagreed. Fixed in `drivers::anthropic`.
- **The prompt cache was unobservable.** `Usage` carried only input/output
  totals, so nothing downstream could tell a cache hit from a miss (identical
  token counts, very different price) or account for reasoning tokens. `Usage`
  now carries `cache_read_input_tokens`, `cache_creation_input_tokens`, and
  `reasoning_tokens` as breakdowns of the totals, populated by both HTTP
  drivers — OpenAI's `prompt_tokens_details` / `completion_tokens_details` were
  being discarded entirely. `session_efficiency` is the regression net.
